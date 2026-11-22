fn read(reader: &mut impl std::io::Read, size: usize) -> std::io::Result<Vec<u8>> {
    let mut out = vec![0; size];
    reader.read_exact(&mut out)?;
    Ok(out)
}

#[derive(Debug, PartialEq)]
pub struct Packet {
    pub id: u32,
    pub opcode: u16,
    pub body: Vec<u8>,
}

// header word1 + word2
const BODY_SIZE_ADJ: u16 = 8;

pub fn read_packet(serial: &mut impl std::io::Read) -> Result<Option<Packet>, &'static str> {
    let header_word1 = match read(serial, 4) {
        Ok(x) => x,
        Err(e) => match e.kind() {
            std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset => {
                return Ok(None);
            }
            _ => {
                eprintln!("kind {:?}", e.kind());
                return Err("header word 1");
            }
        },
    };
    let header_word2 = read(serial, 4).map_err(|_| "header word 2")?;
    let message_size =
        u16::from_ne_bytes(header_word2[2usize..2usize + 2usize].try_into().unwrap());
    if message_size < BODY_SIZE_ADJ {
        return Err("message size too small");
    }
    let body = read(serial, (message_size - BODY_SIZE_ADJ) as usize).map_err(|_| "body")?;
    let opcode = u16::from_ne_bytes(header_word2[0usize..2usize].try_into().unwrap());
    let id = u32::from_ne_bytes(header_word1[0usize..4usize].try_into().unwrap());
    Ok(Some(Packet { id, opcode, body }))
}

pub fn write_packet(serial: &mut impl std::io::Write, data: &Packet) -> Result<(), &'static str> {
    let mut header_word2 = vec![0; 4usize];
    let message_size = (data.body.len() as u16).wrapping_add(BODY_SIZE_ADJ);
    header_word2[2usize..2usize + 2usize].copy_from_slice(&message_size.to_le_bytes());
    header_word2[0usize..2usize].copy_from_slice(&data.opcode.to_le_bytes());
    let mut header_word1 = vec![0; 4usize];
    header_word1[0usize..4usize].copy_from_slice(&data.id.to_le_bytes());
    serial
        .write_all(&header_word1)
        .map_err(|_| "header word 1")?;
    serial
        .write_all(&header_word2)
        .map_err(|_| "header word 2")?;
    serial.write_all(&data.body).map_err(|_| "body")?;
    Ok(())
}

pub fn read_arg_uint(serial: &mut impl std::io::Read) -> Result<u32, &str> {
    let header = read(serial, 4).map_err(|_| "uint")?;
    Ok(u32::from_ne_bytes(header[..].try_into().unwrap()))
}

pub fn write_arg_uint(serial: &mut impl std::io::Write, data: u32) -> Result<(), &str> {
    serial.write_all(&data.to_ne_bytes()).map_err(|_| "arg")?;
    Ok(())
}

pub fn read_arg_string(serial: &mut impl std::io::Read) -> Result<Option<String>, &str> {
    let header = read(serial, 4).map_err(|_| "null terminated string length")?;
    let null_term_len = u32::from_ne_bytes(header[..].try_into().unwrap());
    if null_term_len == 0 {
        return Ok(None);
    }
    let mut body =
        read(serial, null_term_len.next_multiple_of(4) as usize).map_err(|_| "string body")?;
    body.truncate(null_term_len as usize - 1);
    Ok(Some(String::from_utf8(body).map_err(|_| "bad utf-8")?))
}

pub fn write_arg_string(serial: &mut impl std::io::Write, data: &str) -> Result<(), &'static str> {
    let mut buf = data.as_bytes().to_vec();
    buf.push(0);
    let null_term_len = buf.len();
    buf.resize(buf.len().next_multiple_of(4), 0u8);
    serial
        .write_all(&(null_term_len as u32).to_ne_bytes())
        .map_err(|_| "null terminated string length")?;
    serial.write_all(&buf).map_err(|_| "string body")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor, ErrorKind, Read};

    /// Always returns the specified error kind.
    struct FailReader(ErrorKind);

    impl Read for FailReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "simulated i/o error"))
        }
    }

    // ---------------------------------------------------------------------------
    // Packet read / write
    // ---------------------------------------------------------------------------

    #[test]
    fn packet_roundtrip_empty_body() {
        let p = Packet {
            id: 1,
            opcode: 0,
            body: vec![],
        };
        let mut buf = vec![];
        write_packet(&mut buf, &p).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_packet(&mut c).unwrap().unwrap();
        assert_eq!(r.id, 1);
        assert_eq!(r.opcode, 0);
        assert!(r.body.is_empty());
    }

    #[test]
    fn packet_roundtrip_with_body() {
        let p = Packet {
            id: 42,
            opcode: 7,
            body: vec![0xAA, 0xBB, 0xCC, 0xDD],
        };
        let mut buf = vec![];
        write_packet(&mut buf, &p).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_packet(&mut c).unwrap().unwrap();
        assert_eq!(r.id, 42);
        assert_eq!(r.opcode, 7);
        assert_eq!(r.body, vec![0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn packet_odd_body_size() {
        let body = vec![1u8, 2, 3];
        let p = Packet {
            id: 1,
            opcode: 0,
            body: body.clone(),
        };
        let mut buf = vec![];
        write_packet(&mut buf, &p).unwrap();
        assert_eq!(buf.len(), 11);
        let mut c = Cursor::new(&buf);
        let r = read_packet(&mut c).unwrap().unwrap();
        assert_eq!(r.body, body);
    }

    #[test]
    fn packet_large_body() {
        let body = vec![0x42u8; 4096];
        let p = Packet {
            id: 1,
            opcode: 0,
            body: body.clone(),
        };
        let mut buf = vec![];
        write_packet(&mut buf, &p).unwrap();
        assert_eq!(buf.len(), 8 + 4096);
        let mut c = Cursor::new(&buf);
        let r = read_packet(&mut c).unwrap().unwrap();
        assert_eq!(r.body, body);
    }

    #[test]
    fn packet_max_id_and_opcode() {
        let p = Packet {
            id: u32::MAX,
            opcode: u16::MAX,
            body: vec![],
        };
        let mut buf = vec![];
        write_packet(&mut buf, &p).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_packet(&mut c).unwrap().unwrap();
        assert_eq!(r.id, u32::MAX);
        assert_eq!(r.opcode, u16::MAX);
    }

    #[test]
    fn packet_wire_format() {
        let p = Packet {
            id: 1,
            opcode: 2,
            body: vec![3, 0, 0, 0],
        };
        let mut buf = vec![];
        write_packet(&mut buf, &p).unwrap();
        assert_eq!(buf, vec![1, 0, 0, 0, 2, 0, 12, 0, 3, 0, 0, 0]);
    }

    #[test]
    fn packet_multiple_sequential() {
        let packets = vec![
            Packet {
                id: 1,
                opcode: 0,
                body: vec![],
            },
            Packet {
                id: 2,
                opcode: 1,
                body: vec![0x10, 0x00, 0x00, 0x00],
            },
            Packet {
                id: 3,
                opcode: 2,
                body: vec![0x20, 0x00, 0x00, 0x00, 0x30, 0x00, 0x00, 0x00],
            },
        ];
        let mut buf = vec![];
        for p in &packets {
            write_packet(&mut buf, p).unwrap();
        }
        let mut c = Cursor::new(&buf);
        for expected in &packets {
            let r = read_packet(&mut c).unwrap().unwrap();
            assert_eq!(r.id, expected.id, "id mismatch");
            assert_eq!(r.opcode, expected.opcode, "opcode mismatch");
            assert_eq!(r.body, expected.body, "body mismatch");
        }
        assert!(read_packet(&mut c).unwrap().is_none());
    }

    // ---------------------------------------------------------------------------
    // Packet EOF / error handling
    // ---------------------------------------------------------------------------

    #[test]
    fn packet_returns_none_on_empty_reader() {
        let mut c = Cursor::new(vec![]);
        assert!(read_packet(&mut c).unwrap().is_none());
    }

    #[test]
    fn packet_returns_none_on_broken_pipe() {
        let mut r = FailReader(ErrorKind::BrokenPipe);
        assert!(read_packet(&mut r).unwrap().is_none());
    }

    #[test]
    fn packet_returns_none_on_connection_reset() {
        let mut r = FailReader(ErrorKind::ConnectionReset);
        assert!(read_packet(&mut r).unwrap().is_none());
    }

    #[test]
    fn packet_returns_none_on_not_connected() {
        let mut r = FailReader(ErrorKind::NotConnected);
        assert!(read_packet(&mut r).unwrap().is_none());
    }

    #[test]
    fn packet_returns_err_on_generic_error() {
        let mut r = FailReader(ErrorKind::Other);
        assert_eq!(read_packet(&mut r), Err("header word 1"));
    }

    #[test]
    fn packet_returns_err_on_partial_header() {
        let mut c = Cursor::new(vec![0u8; 4]);
        assert_eq!(read_packet(&mut c), Err("header word 2"));
    }

    #[test]
    fn packet_returns_err_on_truncated_body() {
        let mut buf = vec![0u8; 8];
        // message_size is in header_word2[2..4] = buf[6..8]
        buf[6..8].copy_from_slice(&20u16.to_le_bytes());
        let mut c = Cursor::new(buf);
        assert_eq!(read_packet(&mut c), Err("body"));
    }

    // ---------------------------------------------------------------------------
    // Uint read / write
    // ---------------------------------------------------------------------------

    #[test]
    fn uint_roundtrip_various() {
        let values = [
            0u32, 1, 42, 0xFF, 0xFFFF, 0xDEADBEEF, 0xCAFEBABE, 0xFFFFFFFF,
        ];
        for &val in &values {
            let mut buf = vec![];
            write_arg_uint(&mut buf, val).unwrap();
            assert_eq!(buf.len(), 4);
            let mut c = Cursor::new(&buf);
            let r = read_arg_uint(&mut c).unwrap();
            assert_eq!(r, val, "roundtrip failed for 0x{val:08X}");
        }
    }

    #[test]
    fn uint_wire_format() {
        let mut buf = vec![];
        write_arg_uint(&mut buf, 0xDEADBEEF).unwrap();
        assert_eq!(buf, vec![0xEF, 0xBE, 0xAD, 0xDE]);
    }

    #[test]
    fn uint_errors_on_truncated_read() {
        let mut c = Cursor::new(vec![0u8; 3]);
        assert_eq!(read_arg_uint(&mut c), Err("uint"));
    }

    // ---------------------------------------------------------------------------
    // String read / write
    // ---------------------------------------------------------------------------

    #[test]
    fn string_roundtrip_various() {
        let cases: [(&str, Option<&str>); 7] = [
            ("", Some("")),
            ("a", Some("a")),
            ("ab", Some("ab")),
            ("abc", Some("abc")),
            ("abcd", Some("abcd")),
            ("hello", Some("hello")),
            ("a quick brown fox", Some("a quick brown fox")),
        ];
        for (input, expected) in &cases {
            let mut buf = vec![];
            write_arg_string(&mut buf, input).unwrap();
            let mut c = Cursor::new(&buf);
            let r = read_arg_string(&mut c).unwrap();
            assert_eq!(r.as_deref(), *expected, "failed for {input:?}");
        }
    }

    #[test]
    fn string_unicode() {
        let s = "Привет, мир!";
        let mut buf = vec![];
        write_arg_string(&mut buf, s).unwrap();
        let mut c = Cursor::new(&buf);
        let r = read_arg_string(&mut c).unwrap();
        assert_eq!(r, Some(s.to_string()));
    }

    #[test]
    fn string_null() {
        let buf = [0u8; 4];
        let mut c = Cursor::new(&buf[..]);
        let r = read_arg_string(&mut c).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn string_wire_format_simple() {
        let mut buf = vec![];
        write_arg_string(&mut buf, "hi").unwrap();
        assert_eq!(buf, vec![3, 0, 0, 0, b'h', b'i', 0, 0]);
    }

    #[test]
    fn string_padding() {
        // null_term_len = n+1 must be padded to next multiple of 4.
        for n in 0..=3 {
            let s = "x".repeat(n);
            let mut buf = vec![];
            write_arg_string(&mut buf, s.as_str()).unwrap();
            let expected_total = 4 + (n + 1).next_multiple_of(4);
            assert_eq!(buf.len(), expected_total, "wrong padded size for n={n}");
            let mut c = Cursor::new(&buf);
            let r = read_arg_string(&mut c).unwrap();
            assert_eq!(r, Some(s), "roundtrip failed for n={n}");
        }
    }

    #[test]
    fn string_errors_on_truncated_length() {
        let mut c = Cursor::new(vec![0u8; 3]);
        assert_eq!(
            read_arg_string(&mut c),
            Err("null terminated string length")
        );
    }

    #[test]
    fn string_errors_on_truncated_body() {
        let buf = [20u8, 0, 0, 0, 0, 0, 0, 0];
        let mut c = Cursor::new(&buf[..]);
        assert_eq!(read_arg_string(&mut c), Err("string body"));
    }

    #[test]
    fn string_errors_on_bad_utf8() {
        let mut buf = vec![3u8, 0, 0, 0];
        buf.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x00]);
        let mut c = Cursor::new(&buf);
        assert_eq!(read_arg_string(&mut c), Err("bad utf-8"));
    }
}
