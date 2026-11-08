//! Detects non-ISO images (raw 2352/2448 BIN/IMG/MDF, CUE, NRG) and extracts their
//! first data track into a plain 2048-byte-sector ISO that virtdisk can mount.
use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::atomic::{AtomicBool, Ordering::Relaxed},
};
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

const SYNC: [u8; 12] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

pub struct Plan {
    pub src: PathBuf,
    pub offset: u64,
    pub stride: u64,   // bytes per source sector
    pub user_off: u64, // where the 2048 user bytes start inside a sector
    pub sectors: u64,
}

pub enum Source {
    Direct(PathBuf),
    Extract(Plan),
}

fn bad(m: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.into())
}

pub fn prepare(path: &Path) -> io::Result<Source> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "cue" => from_cue(path),
        "nrg" => from_nrg(path),
        "ccd" => sniff(&path.with_extension("img")),
        "mds" => sniff(&path.with_extension("mdf")),
        _ => sniff(path),
    }
}

/// Content-based detection: a raw sector starts with the 12-byte CD sync pattern.
fn sniff(path: &Path) -> io::Result<Source> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = [0u8; 16];
    if len < 16 || f.read_exact(&mut head).is_err() || head[..12] != SYNC {
        return Ok(Source::Direct(path.to_path_buf()));
    }
    let has_sync_at = |f: &mut File, off: u64| -> bool {
        let mut b = [0u8; 12];
        f.seek(SeekFrom::Start(off)).is_ok() && f.read_exact(&mut b).is_ok() && b == SYNC
    };
    let stride = [2352u64, 2448]
        .into_iter()
        .find(|&s| len >= 2 * s && has_sync_at(&mut f, s))
        .unwrap_or(2352);
    Ok(Source::Extract(Plan {
        src: path.to_path_buf(),
        offset: 0,
        stride,
        user_off: raw_user_off(head[15])?,
        sectors: len / stride,
    }))
}

fn raw_user_off(mode: u8) -> io::Result<u64> {
    match mode {
        1 => Ok(16),
        2 => Ok(24), // assumes Form 1 (2048 data bytes)
        m => Err(bad(format!("unknown sector mode ({m})"))),
    }
}

fn mode_layout(mode: &str) -> Option<(u64, u64)> {
    match mode {
        "MODE1/2048" | "MODE2/2048" => Some((2048, 0)),
        "MODE1/2352" => Some((2352, 16)),
        "MODE2/2352" => Some((2352, 24)),
        "MODE2/2336" => Some((2336, 8)),
        _ => None,
    }
}

fn tokens(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (vec![], String::new(), false);
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn msf(s: &str) -> Option<u64> {
    let mut it = s.split(':').map(|x| x.parse::<u64>().ok());
    let (m, s, f) = (it.next()??, it.next()??, it.next()??);
    Some((m * 60 + s) * 75 + f)
}

fn from_cue(cue: &Path) -> io::Result<Source> {
    let bytes = std::fs::read(cue)?;
    // shortcut: non-UTF-8 cues are decoded as Latin-1, other ANSI code pages may mangle names.
    let text = String::from_utf8(bytes.clone()).unwrap_or_else(|_| bytes.iter().map(|&b| b as char).collect());
    let dir = cue.parent().unwrap_or(Path::new("."));
    let (mut file, mut binary) = (None::<PathBuf>, true);
    let mut layout = None::<(u64, u64)>; // current track if it is a data track
    let mut plan = None::<Plan>;
    let mut end = None::<u64>; // first index after the data track, in bytes
    for line in text.lines() {
        let t = tokens(line.trim());
        match t.first().map(|s| s.to_ascii_uppercase()).as_deref() {
            Some("FILE") if t.len() >= 3 => {
                if plan.is_some() {
                    break;
                }
                file = Some(dir.join(&t[1]));
                binary = t[2].eq_ignore_ascii_case("BINARY");
            }
            Some("TRACK") if t.len() >= 3 && plan.is_none() => {
                layout = mode_layout(&t[2].to_ascii_uppercase());
            }
            Some("INDEX") if t.len() >= 3 => {
                let Some(frames) = msf(&t[2]) else { continue };
                if let Some(p) = &plan {
                    end = Some(frames * p.stride);
                    break;
                }
                if t[1] == "01"
                    && let (Some((stride, user_off)), Some(src)) = (layout, file.clone())
                {
                    if !binary {
                        return Err(bad("data track is in a file that is not BINARY"));
                    }
                    plan = Some(Plan { src, offset: frames * stride, stride, user_off, sectors: 0 });
                }
            }
            _ => {}
        }
    }
    let mut p = plan.ok_or_else(|| bad("audio-only images, or images without a data track, are not supported"))?;
    let stop = end.unwrap_or(p.src.metadata()?.len());
    p.sectors = stop.saturating_sub(p.offset) / p.stride;
    Ok(Source::Extract(p))
}

fn be32(b: &[u8]) -> u64 {
    u32::from_be_bytes(b[..4].try_into().unwrap()) as u64
}

fn from_nrg(path: &Path) -> io::Result<Source> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    if len < 12 {
        return Err(bad("invalid NRG file"));
    }
    let mut tail = [0u8; 12];
    f.seek(SeekFrom::Start(len - 12))?;
    f.read_exact(&mut tail)?;
    let chunks = if &tail[..4] == b"NER5" {
        u64::from_be_bytes(tail[4..].try_into().unwrap())
    } else if &tail[4..8] == b"NERO" {
        be32(&tail[8..])
    } else {
        return Err(bad("invalid NRG file"));
    };
    f.seek(SeekFrom::Start(chunks))?;
    loop {
        let mut h = [0u8; 8];
        f.read_exact(&mut h).map_err(|_| bad("NRG file has no data track"))?;
        let size = be32(&h[4..]) as usize;
        let id = [h[0], h[1], h[2], h[3]];
        if &id == b"END!" {
            return Err(bad("NRG file has no data track"));
        }
        let mut body = vec![0u8; size];
        f.read_exact(&mut body)?;
        let (entry, wide) = match &id {
            b"ETNF" => (20, false),
            b"ETN2" => (32, true),
            _ => continue,
        };
        if body.len() < entry {
            return Err(bad("invalid NRG file"));
        }
        let (offset, tsize) = if wide {
            (u64::from_be_bytes(body[..8].try_into().unwrap()), u64::from_be_bytes(body[8..16].try_into().unwrap()))
        } else {
            (be32(&body[..4]), be32(&body[4..8]))
        };
        let mut f2 = File::open(path)?;
        f2.seek(SeekFrom::Start(offset))?;
        let mut head = [0u8; 16];
        let raw = f2.read_exact(&mut head).is_ok() && head[..12] == SYNC;
        let (stride, user_off) = if raw { (2352, raw_user_off(head[15])?) } else { (2048, 0) };
        return Ok(Source::Extract(Plan { src: path.to_path_buf(), offset, stride, user_off, sectors: tsize / stride }));
    }
}

/// Bytes available to the current user on the volume holding `dir` (u64::MAX if unknown).
pub fn free_space(dir: &Path) -> u64 {
    let d = crate::vdisk::wide(dir.as_os_str());
    let mut avail = 0u64;
    let ok = unsafe { GetDiskFreeSpaceExW(d.as_ptr(), &mut avail, null_mut(), null_mut()) };
    if ok == 0 { u64::MAX } else { avail }
}

/// Streams the user data of every sector into `out` using two fixed 1 MiB buffers.
pub fn extract(p: &Plan, out: &Path, cancel: &AtomicBool, progress: &mut dyn FnMut(u8)) -> io::Result<()> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(&p.src)?);
    r.seek(SeekFrom::Start(p.offset))?;
    let mut w = BufWriter::with_capacity(1 << 20, File::create(out)?);
    let (mut buf, n, uo) = ([0u8; 2448], p.stride as usize, p.user_off as usize);
    for i in 0..p.sectors {
        if i % 512 == 0 {
            if cancel.load(Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "canceled"));
            }
            progress((i * 100 / p.sectors) as u8);
        }
        r.read_exact(&mut buf[..n])?;
        w.write_all(&buf[uo..uo + 2048])?;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dim-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn payload(n: usize) -> Vec<u8> {
        (0..n * 2048).map(|i| (i / 2048 * 7 + i % 251) as u8).collect()
    }

    fn wrap(p: &[u8], stride: usize, mode: u8, user_off: usize) -> Vec<u8> {
        let mut out = vec![];
        for s in p.chunks(2048) {
            let mut sec = vec![0u8; stride];
            sec[..12].copy_from_slice(&SYNC);
            sec[15] = mode;
            sec[user_off..user_off + 2048].copy_from_slice(s);
            out.extend(sec);
        }
        out
    }

    fn run(src: Source, d: &Path) -> Vec<u8> {
        let Source::Extract(p) = src else { panic!("expected Extract") };
        let out = d.join("out.iso");
        extract(&p, &out, &AtomicBool::new(false), &mut |_| {}).unwrap();
        std::fs::read(out).unwrap()
    }

    #[test]
    fn raw_variants_even_when_named_iso() {
        let d = dir("raw");
        let pl = payload(3);
        for (stride, mode, uo) in [(2352, 1, 16), (2352, 2, 24), (2448, 1, 16)] {
            let f = d.join("a.iso");
            std::fs::write(&f, wrap(&pl, stride, mode, uo)).unwrap();
            assert_eq!(run(prepare(&f).unwrap(), &d), pl, "stride {stride} mode {mode}");
        }
    }

    #[test]
    fn cooked_is_direct() {
        let d = dir("cooked");
        let f = d.join("a.bin");
        std::fs::write(&f, payload(3)).unwrap();
        assert!(matches!(prepare(&f).unwrap(), Source::Direct(_)));
    }

    #[test]
    fn cue_audio_then_data_with_pregap_and_quotes() {
        let d = dir("cue1");
        let pl = payload(3);
        std::fs::write(d.join("my audio.bin"), vec![9u8; 2352 * 4]).unwrap();
        let mut data = vec![0u8; 2352 * 2]; // 2 sectors of pregap
        data.extend(wrap(&pl, 2352, 1, 16));
        std::fs::write(d.join("data track.bin"), data).unwrap();
        std::fs::write(
            d.join("x.cue"),
            "FILE \"my audio.bin\" BINARY\n TRACK 01 AUDIO\n  INDEX 01 00:00:00\nFILE \"data track.bin\" BINARY\n TRACK 02 MODE1/2352\n  INDEX 00 00:00:00\n  INDEX 01 00:00:02\n",
        )
        .unwrap();
        assert_eq!(run(prepare(&d.join("x.cue")).unwrap(), &d), pl);
    }

    #[test]
    fn cue_data_then_audio_in_same_file_stops_at_next_track() {
        let d = dir("cue2");
        let pl = payload(3);
        let mut bin = wrap(&pl, 2352, 1, 16);
        bin.extend(vec![5u8; 2352 * 4]);
        std::fs::write(d.join("d.bin"), bin).unwrap();
        std::fs::write(
            d.join("x.cue"),
            "FILE d.bin BINARY\n TRACK 01 MODE1/2352\n  INDEX 01 00:00:00\n TRACK 02 AUDIO\n  INDEX 00 00:00:03\n  INDEX 01 00:00:05\n",
        )
        .unwrap();
        assert_eq!(run(prepare(&d.join("x.cue")).unwrap(), &d), pl);
    }

    #[test]
    fn cue_audio_only_is_error() {
        let d = dir("cue3");
        std::fs::write(d.join("a.bin"), vec![1u8; 2352]).unwrap();
        std::fs::write(d.join("x.cue"), "FILE a.bin BINARY\n TRACK 01 AUDIO\n  INDEX 01 00:00:00\n").unwrap();
        assert!(prepare(&d.join("x.cue")).is_err());
    }

    #[test]
    fn nrg_cooked_track() {
        let d = dir("nrg");
        let pl = payload(3);
        let mut f = pl.clone();
        let chunks = f.len() as u64;
        f.extend(b"ETN2");
        f.extend(32u32.to_be_bytes());
        f.extend(0u64.to_be_bytes());
        f.extend((pl.len() as u64).to_be_bytes());
        f.extend([0u8; 16]);
        f.extend(b"END!");
        f.extend(0u32.to_be_bytes());
        f.extend(b"NER5");
        f.extend(chunks.to_be_bytes());
        std::fs::write(d.join("a.nrg"), f).unwrap();
        assert_eq!(run(prepare(&d.join("a.nrg")).unwrap(), &d), pl);
    }
}
