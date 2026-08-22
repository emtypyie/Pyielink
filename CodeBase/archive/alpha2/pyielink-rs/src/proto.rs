use std::io::{Read, Write};

pub const MAX_PAYLOAD: usize = 64 * 1024;

pub const HELLO: u8 = 0x01;
pub const LICENSE_TEXT: u8 = 0x04;
pub const LICENSE_ACCEPT: u8 = 0x05;
pub const LICENSE_REJECT: u8 = 0x06;
pub const TOKEN_ISSUED: u8 = 0x07;
pub const AUTH_OK: u8 = 0x09;
pub const AUTH_FAIL: u8 = 0x0A;
pub const CHALLENGE: u8 = 0x0B;
pub const PROOF: u8 = 0x0C;
pub const PING: u8 = 0x0D;
pub const PONG: u8 = 0x0E;
pub const BYE: u8 = 0x0F;

pub fn write_frame<W: Write>(w: &mut W, msg: u8, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "payload too large",
        ));
    }
    let mut head = [0u8; 3];
    head[0] = msg;
    let len = payload.len() as u16;
    head[1] = (len >> 8) as u8;
    head[2] = len as u8;
    w.write_all(&head)?;
    w.write_all(payload)
}

pub fn read_frame<R: Read>(r: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 3];
    r.read_exact(&mut head)?;
    let msg = head[0];
    let len = ((head[1] as usize) << 8) | head[2] as usize;
    if len > MAX_PAYLOAD {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds max payload",
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((msg, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, HELLO, b"user\n0.1.0").unwrap();
        buf.extend_from_slice(&[TOKEN_ISSUED, 0x00, 0x02, b'a', b'b']);

        let mut cur = Cursor::new(buf);
        let (t, p) = read_frame(&mut cur).unwrap();
        assert_eq!(t, HELLO);
        assert_eq!(p, b"user\n0.1.0");
        let (t2, p2) = read_frame(&mut cur).unwrap();
        assert_eq!(t2, TOKEN_ISSUED);
        assert_eq!(p2, b"ab");
    }

    #[test]
    fn empty_payload_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, CHALLENGE, b"salt\nnonce").unwrap();
        let mut cur = Cursor::new(buf);
        let (t, p) = read_frame(&mut cur).unwrap();
        assert_eq!(t, CHALLENGE);
        assert_eq!(p, b"salt\nnonce");
    }

    #[test]
    fn rejects_oversize_len() {
        let bad = [HELLO, 0xFF, 0xFF];
        let mut cur = Cursor::new(bad.to_vec());
        assert!(read_frame(&mut cur).is_err());
    }

    #[test]
    fn rejects_truncated_stream() {
        let mut buf = Vec::new();
        write_frame(&mut buf, AUTH_OK, b"4243").unwrap();
        buf.pop();
        let mut cur = Cursor::new(buf);
        assert!(read_frame(&mut cur).is_err());
    }
}
