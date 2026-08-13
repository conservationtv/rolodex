use crate::{Error, Result};

const MAX_CHILD_BOXES: usize = 65_536;

#[derive(Debug, Clone, Copy)]
struct RawBoxHeader {
    kind: [u8; 4],
    size: u64,
    header_size: u64,
}

fn decode_box_header(bytes: &[u8], available: u64) -> Result<RawBoxHeader> {
    if bytes.len() < 8 {
        return Err(Error::Invalid("truncated box header"));
    }
    let size32 = read_u32(bytes, 0)? as u64;
    let kind = bytes[4..8].try_into().expect("fixed fourcc slice");
    let (size, header_size) = match size32 {
        0 => (available, 8),
        1 => (read_u64(bytes, 8)?, 16),
        value => (value, 8),
    };
    if size < header_size {
        return Err(Error::Invalid("box is smaller than its header"));
    }
    if size > available {
        return Err(Error::Invalid("box extends beyond its container"));
    }
    Ok(RawBoxHeader {
        kind,
        size,
        header_size,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    pub start: u64,
    pub size: u64,
    pub header_size: u64,
}

impl BoxHeader {
    pub fn end(self) -> Result<u64> {
        self.start
            .checked_add(self.size)
            .ok_or(Error::Invalid("box end overflow"))
    }
}

/// Parse a top-level header from 8 or 16 bytes at `absolute_offset`.
pub fn read_top_level_header(
    bytes: &[u8],
    absolute_offset: u64,
    container_end: u64,
) -> Result<BoxHeader> {
    let available = container_end
        .checked_sub(absolute_offset)
        .ok_or(Error::Invalid("box starts after container"))?;
    let raw = decode_box_header(bytes, available)?;
    let end = absolute_offset
        .checked_add(raw.size)
        .ok_or(Error::Invalid("box end overflow"))?;
    debug_assert!(end <= container_end);
    Ok(BoxHeader {
        kind: raw.kind,
        start: absolute_offset,
        size: raw.size,
        header_size: raw.header_size,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxView {
    pub kind: [u8; 4],
    pub start: usize,
    pub size: usize,
    pub header_size: usize,
}

impl BoxView {
    pub fn payload_start(self) -> usize {
        self.start + self.header_size
    }

    pub fn end(self) -> usize {
        self.start + self.size
    }

    pub fn payload(self, data: &[u8]) -> Result<&[u8]> {
        data.get(self.payload_start()..self.end())
            .ok_or(Error::Invalid("box payload outside buffer"))
    }
}

pub fn box_at(data: &[u8], start: usize, container_end: usize) -> Result<BoxView> {
    if start > container_end || container_end > data.len() {
        return Err(Error::Invalid("invalid box container bounds"));
    }
    let header = data
        .get(start..container_end)
        .ok_or(Error::Invalid("box start outside buffer"))?;
    let raw = decode_box_header(header, (container_end - start) as u64)?;
    let size =
        usize::try_from(raw.size).map_err(|_| Error::Limit("box size does not fit memory"))?;
    let header_size = usize::try_from(raw.header_size).expect("box header is 8 or 16 bytes");
    let end = start
        .checked_add(size)
        .ok_or(Error::Invalid("box end overflow"))?;
    debug_assert!(end <= container_end);
    Ok(BoxView {
        kind: raw.kind,
        start,
        size,
        header_size,
    })
}

pub struct Children<'a> {
    data: &'a [u8],
    cursor: usize,
    end: usize,
    count: usize,
    failed: bool,
}

impl Iterator for Children<'_> {
    type Item = Result<BoxView>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.cursor == self.end {
            return None;
        }
        if self.count >= MAX_CHILD_BOXES {
            self.failed = true;
            return Some(Err(Error::Limit("too many child boxes")));
        }
        match box_at(self.data, self.cursor, self.end) {
            Ok(child) => {
                self.cursor = child.end();
                self.count += 1;
                Some(Ok(child))
            }
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

pub fn children(data: &[u8], parent: BoxView) -> Result<Children<'_>> {
    children_between(data, parent.payload_start(), parent.end())
}

pub fn children_between(data: &[u8], start: usize, end: usize) -> Result<Children<'_>> {
    if start > end || end > data.len() {
        return Err(Error::Invalid("invalid child box bounds"));
    }
    Ok(Children {
        data,
        cursor: start,
        end,
        count: 0,
        failed: false,
    })
}

pub fn child(data: &[u8], parent: BoxView, kind: &[u8; 4]) -> Result<Option<BoxView>> {
    for view in children(data, parent)? {
        let view = view?;
        if &view.kind == kind {
            return Ok(Some(view));
        }
    }
    Ok(None)
}

pub fn read_u16(data: &[u8], offset: usize) -> Result<u16> {
    let bytes: [u8; 2] = data
        .get(offset..offset + 2)
        .ok_or(Error::Invalid("truncated u16"))?
        .try_into()
        .expect("fixed integer slice");
    Ok(u16::from_be_bytes(bytes))
}

pub fn read_u24(data: &[u8], offset: usize) -> Result<u32> {
    let bytes = data
        .get(offset..offset + 3)
        .ok_or(Error::Invalid("truncated u24"))?;
    Ok((u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]))
}

pub fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or(Error::Invalid("truncated u32"))?
        .try_into()
        .expect("fixed integer slice");
    Ok(u32::from_be_bytes(bytes))
}

pub fn read_i32(data: &[u8], offset: usize) -> Result<i32> {
    Ok(read_u32(data, offset)? as i32)
}

pub fn read_i64(data: &[u8], offset: usize) -> Result<i64> {
    Ok(read_u64(data, offset)? as i64)
}

pub fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or(Error::Invalid("truncated u64"))?
        .try_into()
        .expect("fixed integer slice");
    Ok(u64::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extended_box() {
        let data = [0, 0, 0, 1, b'm', b'o', b'o', b'v', 0, 0, 0, 0, 0, 0, 0, 16];
        let view = box_at(&data, 0, data.len()).unwrap();
        let header = read_top_level_header(&data, 0, data.len() as u64).unwrap();
        assert_eq!(view.kind, *b"moov");
        assert_eq!(view.size, 16);
        assert_eq!(view.header_size, 16);
        assert_eq!(header.kind, view.kind);
        assert_eq!(header.size, view.size as u64);
        assert_eq!(header.header_size, view.header_size as u64);
    }

    #[test]
    fn rejects_huge_box_outside_parent() {
        let data = [0xff, 0xff, 0xff, 0xff, b'm', b'd', b'a', b't'];
        assert!(box_at(&data, 0, data.len()).is_err());
        assert!(read_top_level_header(&data, 0, data.len() as u64).is_err());
    }
}
