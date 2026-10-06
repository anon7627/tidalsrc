//! FLAC in fragmented MP4 (ISO/IEC 14496-12, FLAC-in-ISOBMFF mapping), as
//! TIDAL's DASH streams carry it, turned back into a plain FLAC file:
//!
//! - the init segment holds `moov/trak/mdia/minf/stbl/stsd/fLaC/dfLa`: the
//!   FLAC metadata blocks (STREAMINFO first). `fLaC` + those blocks is the
//!   FLAC file's header;
//! - each media segment is `moof` (`traf/tfhd` + `trun`: sample sizes and
//!   where the data starts) then `mdat`. Each sample is one FLAC frame, so
//!   the samples, in order, are the file's frames.
//!
//! Nothing is decoded: the bytes are copied. The only change is in
//! STREAMINFO, when it leaves the total number of samples unknown (0) and
//! the manifest gives it.

use anyhow::{anyhow, bail, ensure, Result};

/// One box: its type and where its body lies in the buffer.
#[derive(Debug, Clone, Copy)]
struct MBox {
    kind: [u8; 4],
    start: usize,
    body: usize,
    end: usize,
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    let s = b.get(at..at + 4).ok_or_else(|| anyhow!("truncated box"))?;
    Ok(u32::from_be_bytes(s.try_into().expect("4 bytes")))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    let s = b.get(at..at + 8).ok_or_else(|| anyhow!("truncated box"))?;
    Ok(u64::from_be_bytes(s.try_into().expect("8 bytes")))
}

/// Boxes between `from` and `to`. With `partial`, a last box running past
/// `to` is still returned (its `end` beyond the buffer).
fn boxes(b: &[u8], from: usize, to: usize, partial: bool) -> Result<Vec<MBox>> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let size32 = u32_at(b, at)? as usize;
        let kind: [u8; 4] = b[at + 4..at + 8].try_into().expect("4 bytes");
        let (size, header) = match size32 {
            1 => (u64_at(b, at + 8)? as usize, 16),
            0 => (to - at, 8),
            n => (n, 8),
        };
        ensure!(size >= header, "bad box size at {at}");
        let end = at.checked_add(size).ok_or_else(|| anyhow!("bad box size at {at}"))?;
        if end > to && !partial {
            bail!("box {} runs past its parent", String::from_utf8_lossy(&kind));
        }
        out.push(MBox { kind, start: at, body: at + header, end });
        if end > to {
            break;
        }
        at = end;
    }
    Ok(out)
}

fn child(b: &[u8], parent: &MBox, kind: &[u8; 4]) -> Result<Option<MBox>> {
    Ok(boxes(b, parent.body, parent.end, false)?.into_iter().find(|x| &x.kind == kind))
}

fn path(b: &[u8], kinds: &[&[u8; 4]]) -> Result<MBox> {
    let mut cur = boxes(b, 0, b.len(), false)?
        .into_iter()
        .find(|x| &x.kind == kinds[0])
        .ok_or_else(|| anyhow!("no {} box", String::from_utf8_lossy(kinds[0])))?;
    for kind in &kinds[1..] {
        cur = child(b, &cur, kind)?.ok_or_else(|| anyhow!("no {} box", String::from_utf8_lossy(*kind)))?;
    }
    Ok(cur)
}

/// The FLAC file's header, from the init segment.
#[derive(Debug, Clone, PartialEq)]
pub struct FlacHeader {
    /// `fLaC` and the metadata blocks, the last one flagged as such.
    pub bytes: Vec<u8>,
    pub sample_rate: u32,
    pub bits: u32,
    pub channels: u32,
    pub total_samples: u64,
}

/// STREAMINFO fields (34-byte block body).
fn streaminfo(si: &[u8]) -> Result<(u32, u32, u32, u64)> {
    ensure!(si.len() >= 34, "STREAMINFO too short");
    let x = u64::from_be_bytes(si[10..18].try_into().expect("8 bytes"));
    let rate = (x >> 44) as u32;
    let channels = ((x >> 41) & 0x7) as u32 + 1;
    let bits = ((x >> 36) & 0x1f) as u32 + 1;
    let total = x & 0xf_ffff_ffff;
    Ok((rate, bits, channels, total))
}

/// Parse the init segment. `total_samples`: the track's length in samples,
/// written into STREAMINFO when it says 0 (unknown).
pub fn flac_header(init: &[u8], total_samples: Option<u64>) -> Result<FlacHeader> {
    let stsd = path(init, &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"])?;
    // Full box: version and flags, then the entry count.
    let entries = boxes(init, stsd.body + 8, stsd.end, false)?;
    let entry = entries.first().ok_or_else(|| anyhow!("stsd without entries"))?;
    match &entry.kind {
        b"fLaC" => {}
        b"enca" => bail!("encrypted stream (DRM)"),
        other => bail!("not FLAC ({})", String::from_utf8_lossy(other)),
    }
    // AudioSampleEntry fields: 28 bytes, then its boxes.
    let dfla = boxes(init, entry.body + 28, entry.end, false)?
        .into_iter()
        .find(|x| &x.kind == b"dfLa")
        .ok_or_else(|| anyhow!("fLaC without dfLa"))?;
    let mut at = dfla.body + 4;
    let mut blocks: Vec<(u8, &[u8])> = Vec::new();
    while at + 4 <= dfla.end {
        let kind = init[at] & 0x7f;
        let last = init[at] & 0x80 != 0;
        let len = (u32_at(init, at)? & 0x00ff_ffff) as usize;
        let body = init.get(at + 4..at + 4 + len).filter(|_| at + 4 + len <= dfla.end).ok_or_else(|| anyhow!("dfLa: truncated block"))?;
        blocks.push((kind, body));
        at += 4 + len;
        if last {
            break;
        }
    }
    ensure!(blocks.first().is_some_and(|(k, b)| *k == 0 && b.len() == 34), "dfLa: no STREAMINFO first");
    let mut si = blocks[0].1.to_vec();
    let (sample_rate, bits, channels, mut total) = streaminfo(&si)?;
    if let (0, Some(n)) = (total, total_samples) {
        if n < (1 << 36) {
            let mut x = u64::from_be_bytes(si[10..18].try_into().expect("8 bytes"));
            x = (x & !0xf_ffff_ffff) | n;
            si[10..18].copy_from_slice(&x.to_be_bytes());
            total = n;
        }
    }
    let mut bytes = b"fLaC".to_vec();
    let count = blocks.len();
    for (i, (kind, body)) in blocks.iter().enumerate() {
        let body: &[u8] = if i == 0 { &si } else { body };
        let flag = if i + 1 == count { 0x80 } else { 0 };
        bytes.push(kind | flag);
        bytes.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        bytes.extend_from_slice(body);
    }
    Ok(FlacHeader { bytes, sample_rate, bits, channels, total_samples: total })
}

/// Where a run of samples lies in the segment, and its length.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Run {
    start: usize,
    len: usize,
}

/// The runs of a segment's first `moof`; `None` when `b` (a prefix of the
/// segment) does not hold the whole `moof` yet.
fn runs(b: &[u8]) -> Result<Option<Vec<Run>>> {
    let top = boxes(b, 0, b.len(), true)?;
    // No `moof` yet: it may come further on.
    let Some(moof) = top.iter().find(|x| &x.kind == b"moof") else { return Ok(None) };
    if moof.end > b.len() {
        return Ok(None);
    }
    let mdat_body = top.iter().find(|x| &x.kind == b"mdat" && x.start >= moof.end).map(|x| x.body);
    let mut out = Vec::new();
    let mut next: Option<usize> = mdat_body;
    for traf in boxes(b, moof.body, moof.end, false)?.iter().filter(|x| &x.kind == b"traf") {
        let parts = boxes(b, traf.body, traf.end, false)?;
        let tfhd = parts.iter().find(|x| &x.kind == b"tfhd").ok_or_else(|| anyhow!("traf without tfhd"))?;
        let flags = u32_at(b, tfhd.body)? & 0x00ff_ffff;
        let mut at = tfhd.body + 8;
        let mut base = moof.start;
        if flags & 0x1 != 0 {
            base = usize::try_from(u64_at(b, at)?)?;
            at += 8;
        }
        if flags & 0x2 != 0 {
            at += 4;
        }
        if flags & 0x8 != 0 {
            at += 4;
        }
        let default_size = if flags & 0x10 != 0 { Some(u32_at(b, at)? as usize) } else { None };
        for trun in parts.iter().filter(|x| &x.kind == b"trun") {
            let tflags = u32_at(b, trun.body)? & 0x00ff_ffff;
            let count = u32_at(b, trun.body + 4)? as usize;
            let mut at = trun.body + 8;
            let mut start = next;
            if tflags & 0x1 != 0 {
                let offset = u32_at(b, at)? as i32;
                start = Some(base.checked_add_signed(offset as isize).ok_or_else(|| anyhow!("bad data offset"))?);
                at += 4;
            }
            if tflags & 0x4 != 0 {
                at += 4;
            }
            let fields = [0x100, 0x200, 0x400, 0x800].iter().filter(|f| tflags & **f != 0).count();
            let mut len = 0usize;
            for i in 0..count {
                let size = if tflags & 0x200 != 0 {
                    let skip = usize::from(tflags & 0x100 != 0) * 4;
                    u32_at(b, at + i * fields * 4 + skip)? as usize
                } else {
                    default_size.ok_or_else(|| anyhow!("trun without sample sizes"))?
                };
                len += size;
            }
            ensure!(at + count * fields * 4 <= trun.end, "trun: truncated");
            let start = start.ok_or_else(|| anyhow!("trun: no data offset and no mdat"))?;
            out.push(Run { start, len });
            next = Some(start + len);
        }
    }
    ensure!(!out.is_empty(), "moof without samples");
    Ok(Some(out))
}

/// Bytes of FLAC frames in a segment, from its beginning (`prefix`); `None`
/// when the prefix is too short to tell.
pub fn audio_len(prefix: &[u8]) -> Result<Option<u64>> {
    Ok(runs(prefix)?.map(|r| r.iter().map(|r| r.len as u64).sum()))
}

/// The FLAC frames of a whole segment.
pub fn audio(segment: &[u8]) -> Result<Vec<u8>> {
    let runs = runs(segment)?.ok_or_else(|| anyhow!("truncated segment"))?;
    let mut out = Vec::with_capacity(runs.iter().map(|r| r.len).sum());
    for r in runs {
        let data = segment.get(r.start..r.start + r.len).ok_or_else(|| anyhow!("samples past the end of the segment"))?;
        out.extend_from_slice(data);
    }
    Ok(out)
}

#[cfg(test)]
pub mod testing {
    //! Builds fragmented MP4 files the way TIDAL serves FLAC, for tests.

    pub fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    fn full(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
        let mut b = flags.to_be_bytes().to_vec();
        b.extend_from_slice(body);
        boxed(kind, &b)
    }

    /// STREAMINFO for 16-bit stereo at `rate`, `total` samples.
    pub fn streaminfo(rate: u32, bits: u32, total: u64) -> Vec<u8> {
        let mut si = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0x10, 0, 0x20, 0];
        let x: u64 = (u64::from(rate) << 44) | (1 << 41) | (u64::from(bits - 1) << 36) | total;
        si.extend_from_slice(&x.to_be_bytes());
        si.extend_from_slice(&[0xaa; 16]);
        si
    }

    /// Init segment with these metadata blocks after STREAMINFO.
    pub fn init(streaminfo: &[u8], others: &[(u8, &[u8])]) -> Vec<u8> {
        let mut blocks = vec![0u8];
        blocks.extend_from_slice(&(streaminfo.len() as u32).to_be_bytes()[1..]);
        blocks.extend_from_slice(streaminfo);
        for (i, (kind, body)) in others.iter().enumerate() {
            // The last-block flag is set on the last one.
            blocks.push(kind | if i + 1 == others.len() { 0x80 } else { 0 });
            blocks.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            blocks.extend_from_slice(body);
        }
        if others.is_empty() {
            blocks[0] |= 0x80;
        }
        let dfla = full(b"dfLa", 0, &blocks);
        let mut entry = vec![0u8; 28];
        entry.extend(dfla);
        let flac = boxed(b"fLaC", &entry);
        let mut stsd_body = 1u32.to_be_bytes().to_vec();
        stsd_body.extend(flac);
        let stsd = full(b"stsd", 0, &stsd_body);
        let nest = |kind: &[u8; 4], inner: Vec<u8>| boxed(kind, &inner);
        let moov = nest(b"moov", nest(b"trak", nest(b"mdia", nest(b"minf", nest(b"stbl", stsd)))));
        let mut file = boxed(b"ftyp", b"iso6dash");
        file.extend(moov);
        file
    }

    /// A media segment holding `frames`, one sample each.
    pub fn segment(frames: &[&[u8]]) -> Vec<u8> {
        let mut tfhd = 1u32.to_be_bytes().to_vec(); // track id
        tfhd.splice(0..0, 0x02_0000u32.to_be_bytes()); // default-base-is-moof
        let tfhd = boxed(b"tfhd", &tfhd);
        let trun_len = 8 + 4 + 4 + 4 + frames.len() * 8;
        let traf_len = 8 + tfhd.len() + trun_len;
        let moof_len = 8 + 16 + traf_len;
        let mut trun = 0x0000_0301u32.to_be_bytes().to_vec(); // data offset, duration, size
        trun.extend_from_slice(&(frames.len() as u32).to_be_bytes());
        trun.extend_from_slice(&((moof_len + 8) as u32).to_be_bytes());
        for f in frames {
            trun.extend_from_slice(&4096u32.to_be_bytes());
            trun.extend_from_slice(&(f.len() as u32).to_be_bytes());
        }
        let trun = boxed(b"trun", &trun);
        let mut traf = tfhd;
        traf.extend(trun);
        let mut moof = boxed(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        moof.extend(boxed(b"traf", &traf));
        let moof = boxed(b"moof", &moof);
        assert_eq!(moof.len(), moof_len);
        // The data offset counts from the moof, which follows `styp`.
        let mut file = boxed(b"styp", b"msdh");
        file.extend(moof);
        file.extend(boxed(b"mdat", &frames.concat()));
        file
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_segment_gives_the_flac_header() {
        let si = testing::streaminfo(96_000, 24, 0);
        let init = testing::init(&si, &[(4, b"vorbis comment"), (1, &[0; 8])]);
        let h = flac_header(&init, Some(873_000)).unwrap();
        assert_eq!((h.sample_rate, h.bits, h.channels, h.total_samples), (96_000, 24, 2, 873_000));
        assert!(h.bytes.starts_with(b"fLaC\x00\x00\x00\x22"));
        assert_eq!(h.bytes.len(), 4 + 4 + 34 + 4 + 14 + 4 + 8);
        assert_eq!(h.bytes[4 + 38 + 18], 0x81, "last block flagged");
        let (_, _, _, total) = streaminfo(&h.bytes[8..42]).unwrap();
        assert_eq!(total, 873_000, "unknown length filled in");
        // A known length is left alone.
        let h = flac_header(&testing::init(&testing::streaminfo(44_100, 16, 500), &[]), Some(9)).unwrap();
        assert_eq!((h.sample_rate, h.bits, h.total_samples), (44_100, 16, 500));
        assert_eq!(h.bytes[4], 0x80, "STREAMINFO alone: last");
        assert_eq!(&h.bytes[8..42], testing::streaminfo(44_100, 16, 500).as_slice(), "bytes unchanged");

        let mut enc = testing::init(&si, &[]);
        let pos = enc.windows(4).position(|w| w == b"fLaC").unwrap();
        enc[pos..pos + 4].copy_from_slice(b"enca");
        assert!(flac_header(&enc, None).unwrap_err().to_string().contains("DRM"));
        assert!(flac_header(b"junk", None).is_err());
    }

    #[test]
    fn segments_give_their_frames() {
        let frames: [&[u8]; 3] = [b"\xff\xf8one", b"\xff\xf8second", b"\xff\xf8third frame"];
        let seg = testing::segment(&frames);
        assert_eq!(audio(&seg).unwrap(), frames.concat());
        assert_eq!(audio_len(&seg).unwrap(), Some(frames.concat().len() as u64));
        // From a prefix holding the moof, without the samples.
        let mdat_at = seg.windows(4).position(|w| w == b"mdat").unwrap() + 4;
        assert_eq!(audio_len(&seg[..mdat_at]).unwrap(), Some(frames.concat().len() as u64));
        assert_eq!(audio_len(&seg[..40]).unwrap(), None, "moof not complete yet");
        assert_eq!(audio_len(&seg[..12]).unwrap(), None, "moof not started yet");
        assert!(audio(&seg[..seg.len() - 2]).is_err(), "truncated");
        assert!(audio(&testing::boxed(b"free", b"xx")).is_err());
    }
}
