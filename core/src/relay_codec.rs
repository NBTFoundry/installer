//! Wire primitives recovered from the shipped binary's relay serializer.
//!
//! Meanings of client tags 3, 4 and 6 are not yet established. Do not use
//! this module as evidence that an activation sequence is fully understood.

#[derive(Debug, PartialEq, Eq)]
pub enum ServerMessage {
    Data(Vec<u8>),
    Status { level: u8, text: String },
    Flag(bool),
}

fn field(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), &'static str> {
    let len = u32::try_from(bytes.len()).map_err(|_| "field exceeds u32 length")?;
    output.extend_from_slice(&len.to_be_bytes());
    output.extend_from_slice(bytes);
    Ok(())
}

/// Encode the client-side tag shapes observed at 0x32974d and 0x329861.
pub fn encode_client(tag: u8, fields: &[&[u8]]) -> Result<Vec<u8>, &'static str> {
    let expected = match tag {
        1 => 3,
        2..=4 | 6 => 1,
        5 => 0,
        _ => return Err("unknown client tag"),
    };
    if fields.len() != expected {
        return Err("incorrect field count");
    }
    let mut output = vec![tag];
    for item in fields {
        field(&mut output, item)?;
    }
    Ok(output)
}

pub fn decode_server(data: &[u8]) -> Result<ServerMessage, &'static str> {
    let (&tag, mut rest) = data.split_first().ok_or("missing tag")?;
    fn take_byte(rest: &mut &[u8]) -> Result<u8, &'static str> {
        let (&value, tail) = rest.split_first().ok_or("message ended early")?;
        *rest = tail;
        Ok(value)
    }
    fn take_field<'a>(rest: &mut &'a [u8]) -> Result<&'a [u8], &'static str> {
        let length = rest.get(..4).ok_or("missing length")?;
        let length = u32::from_be_bytes(length.try_into().unwrap()) as usize;
        *rest = &rest[4..];
        let (item, tail) = rest.split_at_checked(length).ok_or("message ended early")?;
        *rest = tail;
        Ok(item)
    }
    let message = match tag {
        0x81 => ServerMessage::Data(take_field(&mut rest)?.to_vec()),
        0x82 => {
            let level = take_byte(&mut rest)?;
            if level >= 5 {
                return Err("unknown status level");
            }
            let text = std::str::from_utf8(take_field(&mut rest)?)
                .map_err(|_| "status is not UTF-8")?
                .to_owned();
            ServerMessage::Status { level, text }
        }
        0x83 => ServerMessage::Flag(take_byte(&mut rest)? != 0),
        _ => return Err("unknown server tag"),
    };
    if !rest.is_empty() {
        return Err("trailing data");
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_tags_and_big_endian_field_lengths() {
        assert_eq!(
            encode_client(2, &[b"ssh_enable"]).unwrap(),
            b"\x02\0\0\0\x0assh_enable"
        );
        assert_eq!(
            encode_client(1, &[b"a", b"bc", b""]).unwrap(),
            b"\x01\0\0\0\x01a\0\0\0\x02bc\0\0\0\0"
        );
        assert!(encode_client(5, &[b"bad"]).is_err());
    }
    #[test]
    fn decodes_server_and_rejects_incomplete_or_unknown_messages() {
        assert_eq!(
            decode_server(b"\x81\0\0\0\x02\x12\x34"),
            Ok(ServerMessage::Data(vec![0x12, 0x34]))
        );
        assert_eq!(
            decode_server(b"\x82\x02\0\0\0\x02ok"),
            Ok(ServerMessage::Status {
                level: 2,
                text: "ok".into()
            })
        );
        assert_eq!(decode_server(b"\x83\x01"), Ok(ServerMessage::Flag(true)));
        for invalid in [
            b"\x81\0\0\0\x03xy".as_slice(),
            b"\x82\x05\0\0\0\0",
            b"\x83\x01\0",
            b"\xff",
        ] {
            assert!(decode_server(invalid).is_err());
        }
    }
}
