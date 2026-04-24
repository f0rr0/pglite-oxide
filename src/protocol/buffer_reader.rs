use anyhow::{Result, ensure};

#[derive(Debug, Default)]
pub struct BufferReader<'a> {
    buffer: &'a [u8],
    offset: usize,
}

impl<'a> BufferReader<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_buffer(&mut self, offset: usize, buffer: &'a [u8]) {
        self.offset = offset;
        self.buffer = buffer;
    }

    fn take_slice(&mut self, len: usize) -> Result<&'a [u8]> {
        ensure!(
            self.offset + len <= self.buffer.len(),
            "buffer underflow (need {len} bytes, have {})",
            self.buffer.len().saturating_sub(self.offset)
        );
        let slice = &self.buffer[self.offset..self.offset + len];
        self.offset += len;
        Ok(slice)
    }

    pub fn int16(&mut self) -> Result<i16> {
        let bytes = self.take_slice(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub fn byte(&mut self) -> Result<u8> {
        let bytes = self.take_slice(1)?;
        Ok(bytes[0])
    }

    pub fn int32(&mut self) -> Result<i32> {
        let bytes = self.take_slice(4)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn string(&mut self, length: usize) -> Result<String> {
        let bytes = self.take_slice(length)?;
        let text = std::str::from_utf8(bytes)?;
        Ok(text.to_owned())
    }

    pub fn cstring(&mut self) -> Result<String> {
        let start = self.offset;
        loop {
            let next = self
                .buffer
                .get(self.offset)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unterminated cstring"))?;
            self.offset += 1;
            if next == 0 {
                let slice = &self.buffer[start..self.offset - 1];
                let text = std::str::from_utf8(slice)?;
                return Ok(text.to_owned());
            }
        }
    }

    pub fn bytes(&mut self, length: usize) -> Result<Vec<u8>> {
        let bytes = self.take_slice(length)?;
        Ok(bytes.to_vec())
    }
}
