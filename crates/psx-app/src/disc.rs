//! Disc image loading: raw `.bin` tracks and `.cue` sheets.
//!
//! Shared by the CLI (`--disc`) and the GUI's disc picker, so failures are
//! returned rather than fatal: the running emulator keeps its current disc
//! when a pick turns out to be unreadable.

use psx_core::cdrom::Disc;
use std::path::Path;

/// What the frontend shows about the disc in the drive.
#[derive(Clone, Debug)]
pub struct DiscInfo {
    /// File name of the image, for the status bar. The full path is too wide
    /// for it, and the directory rarely identifies the disc.
    pub file: String,
    /// Best name the disc itself yields: its ISO9660 volume identifier, or
    /// the file stem when the disc carries none.
    pub title: String,
}

/// Load a disc image: a raw .bin (single data track), or a .cue sheet
/// (multi-track, multi-file), together with the [`DiscInfo`] the UI names it
/// by. The core never looks at that info.
pub fn load_disc(path: &Path) -> Result<(Disc, DiscInfo), String> {
    let disc = read_disc(path)?;
    let lossy = |s: &std::ffi::OsStr| s.to_string_lossy().into_owned();
    let file = path
        .file_name()
        .map_or_else(|| path.display().to_string(), lossy);
    let title = volume_label(&disc)
        .or_else(|| path.file_stem().map(lossy))
        .unwrap_or_else(|| file.clone());
    Ok((disc, DiscInfo { file, title }))
}

/// The ISO9660 volume identifier of the data track, or `None` when the disc
/// has no primary volume descriptor (a pure audio disc) or leaves the field
/// blank.
///
/// PS1 discs carry no human-readable game title anywhere, so this label — in
/// practice the release serial, or the publisher's working name — is as close
/// as the disc alone gets to one.
fn volume_label(disc: &Disc) -> Option<String> {
    // The PVD sits 16 sectors into the filesystem, i.e. past track 1's pregap.
    let start = disc.tracks().first()?.start;
    let pvd = disc.user_data(start + 16)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let label: String = pvd[40..72]
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { ' ' })
        .collect();
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_string())
}

fn read_disc(path: &Path) -> Result<Disc, String> {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
    {
        let cue = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read cue '{}': {e}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        parse_cue(&cue, dir).map_err(|e| format!("bad cue sheet '{}': {e}", path.display()))
    } else {
        let data = std::fs::read(path)
            .map_err(|e| format!("failed to read disc image '{}': {e}", path.display()))?;
        Disc::new(data)
    }
}

/// Parse a cue sheet into one concatenated image plus its TOC. PREGAP
/// directives are materialized as zero-filled sectors so the reported track
/// starts match the assembled layout. Only 2352-byte-sector modes are
/// supported (MODE1/2352, MODE2/2352, AUDIO) — 2048-byte images would need
/// sector reconstruction.
fn parse_cue(cue: &str, dir: &Path) -> Result<Disc, String> {
    use psx_core::cdrom::{RAW_SECTOR, Track};
    struct CueTrack {
        number: u8,
        audio: bool,
        pregap: u32,
        index1: u32,
    }
    let mut files: Vec<(std::path::PathBuf, Vec<CueTrack>)> = Vec::new();
    for (n, line) in cue.lines().enumerate() {
        let err = |m: String| format!("line {}: {m}", n + 1);
        let mut it = line.split_whitespace();
        match it.next() {
            Some("FILE") => {
                let name = line
                    .split('"')
                    .nth(1)
                    .ok_or_else(|| err("FILE needs a quoted name".into()))?;
                files.push((dir.join(name), Vec::new()));
            }
            Some("TRACK") => {
                let file = files
                    .last_mut()
                    .ok_or_else(|| err("TRACK before FILE".into()))?;
                let number: u8 = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| err("bad track number".into()))?;
                let mode = it.next().ok_or_else(|| err("missing track mode".into()))?;
                let audio = mode.eq_ignore_ascii_case("AUDIO");
                if !audio && !mode.ends_with("/2352") {
                    return Err(err(format!(
                        "unsupported track mode {mode} (only 2352-byte sectors)"
                    )));
                }
                file.1.push(CueTrack {
                    number,
                    audio,
                    pregap: 0,
                    index1: 0,
                });
            }
            Some("PREGAP") => {
                let t = files
                    .last_mut()
                    .and_then(|f| f.1.last_mut())
                    .ok_or_else(|| err("PREGAP outside a TRACK".into()))?;
                t.pregap = parse_msf(it.next().unwrap_or(""))
                    .ok_or_else(|| err("bad PREGAP time".into()))?;
            }
            Some("INDEX") => {
                let idx = it.next();
                let t = files
                    .last_mut()
                    .and_then(|f| f.1.last_mut())
                    .ok_or_else(|| err("INDEX outside a TRACK".into()))?;
                if idx == Some("01") {
                    t.index1 = parse_msf(it.next().unwrap_or(""))
                        .ok_or_else(|| err("bad INDEX time".into()))?;
                }
            }
            _ => {} // REM, CATALOG, FLAGS, ...
        }
    }

    let mut data = Vec::new();
    let mut tracks = Vec::new();
    for (path, cue_tracks) in files {
        let file_data = std::fs::read(&path)
            .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
        if file_data.is_empty() || !file_data.len().is_multiple_of(RAW_SECTOR) {
            return Err(format!(
                "'{}' is not a multiple of {RAW_SECTOR} bytes",
                path.display()
            ));
        }
        for (i, t) in cue_tracks.iter().enumerate() {
            if t.pregap > 0 {
                if i > 0 {
                    // Silence would have to be spliced into the middle of
                    // the file's sectors; no real dump needs this.
                    return Err("PREGAP on a later track of a shared FILE is unsupported".into());
                }
                data.resize(data.len() + t.pregap as usize * RAW_SECTOR, 0);
            }
        }
        let base = (data.len() / RAW_SECTOR) as u32;
        for t in &cue_tracks {
            tracks.push(Track {
                number: t.number,
                audio: t.audio,
                start: base + t.index1,
            });
        }
        data.extend_from_slice(&file_data);
    }
    Disc::with_tracks(data, tracks)
}

/// "mm:ss:ff" -> frame count.
fn parse_msf(s: &str) -> Option<u32> {
    let mut it = s.split(':');
    let mm: u32 = it.next()?.parse().ok()?;
    let ss: u32 = it.next()?.parse().ok()?;
    let ff: u32 = it.next()?.parse().ok()?;
    Some((mm * 60 + ss) * 75 + ff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_core::cdrom::RAW_SECTOR;

    #[test]
    fn cue_with_two_files_and_pregap_builds_the_toc() {
        let dir = std::env::temp_dir().join("ps1e-cue-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.bin"), vec![1u8; RAW_SECTOR * 10]).unwrap();
        std::fs::write(dir.join("audio.bin"), vec![2u8; RAW_SECTOR * 5]).unwrap();
        let cue = r#"
FILE "data.bin" BINARY
  TRACK 01 MODE2/2352
    INDEX 01 00:00:00
FILE "audio.bin" BINARY
  TRACK 02 AUDIO
    PREGAP 00:02:00
    INDEX 01 00:00:00
"#;
        let disc = parse_cue(cue, &dir).unwrap();
        let tracks = disc.tracks();
        assert_eq!(tracks.len(), 2);
        assert!(!tracks[0].audio && tracks[0].start == 0);
        // Audio starts after 10 data sectors + 2s (150 sectors) of pregap
        assert!(tracks[1].audio);
        assert_eq!(tracks[1].number, 2);
        assert_eq!(tracks[1].start, 160);
    }

    #[test]
    fn cue_with_one_file_per_disc_still_parses() {
        let dir = std::env::temp_dir().join("ps1e-cue-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("game.bin"), vec![0u8; RAW_SECTOR * 4]).unwrap();
        let cue = "FILE \"game.bin\" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n";
        let disc = parse_cue(cue, &dir).unwrap();
        assert_eq!(disc.tracks().len(), 1);
    }

    /// 17 mode-2 form-1 sectors with an ISO9660 PVD carrying `label`.
    fn iso_image(label: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; RAW_SECTOR * 17];
        let pvd = 16 * RAW_SECTOR + 0x18;
        img[16 * RAW_SECTOR + 0x0f] = 2; // mode 2
        img[pvd] = 1; // primary volume descriptor
        img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        img[pvd + 40..pvd + 72].fill(b' ');
        img[pvd + 40..pvd + 40 + label.len()].copy_from_slice(label);
        img
    }

    #[test]
    fn volume_label_comes_from_the_primary_volume_descriptor() {
        let disc = Disc::new(iso_image(b"SLUS_007.47")).unwrap();
        assert_eq!(volume_label(&disc).as_deref(), Some("SLUS_007.47"));
    }

    #[test]
    fn a_blank_volume_label_is_no_label() {
        let disc = Disc::new(iso_image(b"")).unwrap();
        assert_eq!(volume_label(&disc), None);
    }

    #[test]
    fn a_disc_without_a_filesystem_has_no_label() {
        let disc = Disc::new(vec![0u8; RAW_SECTOR * 17]).unwrap();
        assert_eq!(volume_label(&disc), None);
    }

    #[test]
    fn cue_rejects_2048_byte_sector_modes() {
        let cue = "FILE \"x.bin\" BINARY\n  TRACK 01 MODE1/2048\n";
        let e = parse_cue(cue, Path::new(".")).err().unwrap();
        assert!(e.contains("unsupported track mode"));
    }
}
