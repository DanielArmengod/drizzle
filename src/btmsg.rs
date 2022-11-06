
use std::fs::OpenOptions;
use std::io;
use std::io::{BufWriter, Write};
use std::net::TcpStream;
use hex_literal::hex;
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};

use thiserror::Error;

#[derive(PartialEq, Debug)]
pub enum BtMsg {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    Uninterested,
    HavePiece { idx: u32 },
    Bitfield { bitfield: Vec<u8>},
    BlockRequest { piece_idx: u32, begin: u32, length: u32 },
    BlockResponse { piece_idx: u32, begin: u32, data: Vec<u8> },
    CancelBlockRequest { piece_idx: u32, begin: u32, length: u32 },
}

#[derive(FromPrimitive, ToPrimitive, Debug)]
enum BtMsgKind {
    CHOKE = 0,
    UNCHOKE,
    INTERESTED,
    UNINTERESTED,
    HAVE,
    BITFIELD,
    REQUEST,
    PIECE,
    CANCEL,
}

macro_rules! dsrlz {  // Returns an integer of type `ty`
    ($source:expr, $type:ty) => {
        {
            let source = &mut $source;  // This is a macro, we have to be careful about not double-evaluating stuff. $source could be File::open("/path/...").unwrap()
            let mut buf = [0u8; std::mem::size_of::<$type>()];
            match source.read_exact(buf.as_mut_slice()) {
                Ok(_) => Ok(<$type>::from_be_bytes(buf)),
                Err(e) => Err(e),
            }
        }
    }
}

macro_rules! srlz {
    ($val:expr, $dest:expr) => {  // Returns std::io::Result<()>
        {
            let dest = &mut $dest;
            dest.write_all($val.to_be_bytes().as_slice())
        }
    }
}

#[derive(Error, Debug)]
#[error("placeholder Display impl")]
pub enum BtPeerMsgDecodeError {
    MalformedMsg_BadMsgID,
    IOError(#[from] std::io::Error),
}

static HANDSHAKE_TMPL : &[u8] = include_bytes!("../assets/handshake-template").as_slice();  // Is a &[u8] of length 49+19; it is pre-filled with the entire handshake except for the 20 bytes that correspond to the torrent's info_hash and which must be filled in.

pub fn send_handshake<W: std::io::Write>(info_hash: &[u8], mut data_stream: W) -> io::Result<()> {
    assert_eq!(info_hash.len(), 20);
    let mut handshake_buf = [0u8; 49 + 19];  // "The handshake is [...] 49+19 bytes long." from btspecification
    let handshake_tmpl = HANDSHAKE_TMPL;
    handshake_buf.copy_from_slice(handshake_tmpl);
    let mut info_hash_slice = &mut handshake_buf[28..48];  // I fucking hope my math is right.
    info_hash_slice.copy_from_slice(info_hash);

    data_stream.write_all(handshake_buf.as_slice())
}

pub fn recv_and_check_handshake<R: std::io::Read>(expected_info_hash: &[u8], expected_peer_id: Option<&[u8]>, mut data_stream: R) -> io::Result<()> {  // TODO: WRONG ERROR KIND
    assert_eq!(expected_info_hash.len(), 20);
    // TODO: THE REST OF THE assert_eq!()s IN THIS FUNCTION HAVE TO BE REPLACED BY SOMETHING THAT RETURNS AN ERROR TO THE CALLER INSTEAD.
    let mut handshake_buf = [0u8; 49 + 19];  // "The handshake is [...] 49+19 bytes long." from btspecification
    data_stream.read_exact(handshake_buf.as_mut_slice())?;
    let handshake_tmpl = HANDSHAKE_TMPL;
    assert_eq!(&handshake_tmpl[..20], &handshake_buf[..20]);  // The len(pstr) byte and the pstr are a fixed part of the protocol
    {} // I don't know any sane way to the validity of the 8 reserved bytes yet.
    assert_eq!(expected_info_hash, &handshake_buf[28..48]);
    if let Some(expected_peer_id) = expected_peer_id {
        assert_eq!(expected_peer_id, &handshake_buf[48..]);
    }

    Ok(())
}


impl BtMsg {
    pub fn deserialize<R: std::io::Read>(mut data_stream: R) -> Result<Self, BtPeerMsgDecodeError> {
        let msg_len: u32 = dsrlz!(&mut data_stream, u32)?;
        if msg_len == 0 {
            return Ok(Self::KeepAlive);
        }

        let msg_id: u8 = dsrlz!(&mut data_stream, u8)?;

        use self::BtMsgKind::*;
        let msg_kind = FromPrimitive::from_u8(msg_id).ok_or(BtPeerMsgDecodeError::MalformedMsg_BadMsgID)?;
        Ok(match msg_kind {
            CHOKE => Self::Choke,
            UNCHOKE => Self::Unchoke,
            INTERESTED => Self::Interested,
            UNINTERESTED => Self::Uninterested,
            HAVE => {
                let idx: u32 = dsrlz!(&mut data_stream, u32)?;
                Self::HavePiece { idx }
            }
            BITFIELD => {
                let mut bitfield = vec![0u8; (msg_len - 1) as usize];
                data_stream.read_exact(bitfield.as_mut_slice())?;
                Self::Bitfield { bitfield }
            }
            REQUEST => {
                let (piece_idx, begin, length) = (
                    dsrlz!(&mut data_stream, u32)?,
                    dsrlz!(&mut data_stream, u32)?,
                    dsrlz!(&mut data_stream, u32)?,
                );
                Self::BlockRequest { piece_idx, begin, length }
            }
            PIECE => {
                let (piece_idx, begin) = (
                    dsrlz!(&mut data_stream, u32)?,
                    dsrlz!(&mut data_stream, u32)?,
                );
                let mut data = vec![0u8; (msg_len - 1 - 4 - 4) as usize];
                data_stream.read_exact(data.as_mut_slice())?;
                Self::BlockResponse { piece_idx, begin, data }
            }
            CANCEL => {
                let (piece_idx, begin, length) = (
                    dsrlz!(&mut data_stream, u32)?,
                    dsrlz!(&mut data_stream, u32)?,
                    dsrlz!(&mut data_stream, u32)?,
                );
                Self::CancelBlockRequest { piece_idx, begin, length }
            }
        })
    }

    fn wire_len_and_msgkind(&self) -> (u32, Option<BtMsgKind>) {
        use BtMsg::*;
        use BtMsgKind::*;
        let (msg_len, msg_kind) =  match self {
            &KeepAlive => return (0, None),
            &Choke => (1, CHOKE),
            &Unchoke => (1, UNCHOKE),
            &Interested => (1, INTERESTED),
            &Uninterested => (1, UNINTERESTED),
            &HavePiece {..} => (5, HAVE),
            &BlockRequest {..} => (13, REQUEST),
            &CancelBlockRequest {..} => (13, CANCEL),
            &Bitfield {ref bitfield} => (1+(bitfield.len() as u32), BITFIELD),
            &BlockResponse {ref data, ..} => (9+(data.len() as u32), PIECE),
        };
        return (msg_len, Some(msg_kind));
    }

    pub fn serialize<W: std::io::Write>(&self, mut _data_stream: W) -> io::Result<()> {

        let mut data_stream = BufWriter::new(_data_stream);
        let (msg_len, msg_kind) = self.wire_len_and_msgkind();
        if let None = msg_kind {
            assert_eq!(msg_len, 0);
            return srlz!(msg_len, &mut data_stream);
        }
        let msg_kind = ToPrimitive::to_u8(&msg_kind.unwrap()).unwrap();
        srlz!(msg_len, &mut data_stream)?;
        srlz!(msg_kind, &mut data_stream)?;

        use BtMsg::*;
        match self {
            &KeepAlive | &Choke | &Unchoke | &Interested | &Uninterested => {}
            &HavePiece {idx} => {
                srlz!(idx, &mut data_stream)?
            }
            &BlockRequest {piece_idx, begin, length}
            | &CancelBlockRequest {piece_idx, begin, length} => {
                srlz!(piece_idx, &mut data_stream)?;
                srlz!(begin, &mut data_stream)?;
                srlz!(length, &mut data_stream)?;
            }
            &Bitfield {ref bitfield} => {
                data_stream.write_all(bitfield.as_slice())?
            }
            &BlockResponse {piece_idx, begin, ref data} => {
                srlz!(piece_idx, &mut data_stream)?;
                srlz!(begin, &mut data_stream)?;
                data_stream.write_all(data.as_slice())?;
            }
        }
        data_stream.flush().unwrap();
        Ok(())
    }
}

#[test]
fn test_decode() {
    use std::io::Cursor;
    let mut buffer_for_sent_msg = [0u8;200];

    let msg = include_bytes!("../btmsg-samples/keepalive").as_slice();
    let res = BtMsg::deserialize(msg).unwrap();
    assert_eq!(res, BtMsg::KeepAlive);
    let mut cursor = Cursor::new(buffer_for_sent_msg.as_mut_slice());
    res.serialize(&mut cursor).unwrap();
    let msg_sent = {
        let len = cursor.position() as usize;
        &buffer_for_sent_msg[..len]
    };
    assert_eq!(msg, msg_sent);


    let msg = include_bytes!("../btmsg-samples/choke").as_slice();
    let res = BtMsg::deserialize(msg).unwrap();
    assert_eq!(res, BtMsg::Choke);
    let mut cursor = Cursor::new(buffer_for_sent_msg.as_mut_slice());
    res.serialize(&mut cursor).unwrap();
    let msg_sent = {
        let len = cursor.position() as usize;
        &buffer_for_sent_msg[..len]
    };
    assert_eq!(msg, msg_sent);

    let msg = include_bytes!("../btmsg-samples/unchoke").as_slice();
    let res = BtMsg::deserialize(msg).unwrap();
    assert_eq!(res, BtMsg::Unchoke);
    let mut cursor = Cursor::new(buffer_for_sent_msg.as_mut_slice());
    res.serialize(&mut cursor).unwrap();
    let msg_sent = {
        let len = cursor.position() as usize;
        &buffer_for_sent_msg[..len]
    };
    assert_eq!(msg, msg_sent);

    let msg = include_bytes!("../btmsg-samples/have-piece-42").as_slice();
    let res = BtMsg::deserialize(msg).unwrap();
    assert_eq!(res, BtMsg::HavePiece {idx: 42});
    let mut cursor = Cursor::new(buffer_for_sent_msg.as_mut_slice());
    res.serialize(&mut cursor).unwrap();
    let msg_sent = {
        let len = cursor.position() as usize;
        &buffer_for_sent_msg[..len]
    };
    assert_eq!(msg, msg_sent);

    let msg = include_bytes!("../btmsg-samples/bitfield-10bytes-incomplete").as_slice();
    let res = BtMsg::deserialize(msg).unwrap();
    assert_eq!(res, BtMsg::Bitfield {bitfield: vec![0xFFu8, 0xFF, 0xFF, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0]});
    let mut cursor = Cursor::new(buffer_for_sent_msg.as_mut_slice());
    res.serialize(&mut cursor).unwrap();
    let msg_sent = {
        let len = cursor.position() as usize;
        &buffer_for_sent_msg[..len]
    };
    assert_eq!(msg, msg_sent);

    let msg = include_bytes!("../btmsg-samples/bitfield-10bytes-complete").as_slice();
    let res = BtMsg::deserialize(msg).unwrap();
    assert_eq!(res, BtMsg::Bitfield {bitfield: vec![0xFFu8; 10]});
    let mut used = buffer_for_sent_msg.as_mut_slice();  // Different than the others because I felt cute.
    res.serialize(&mut used).unwrap();
    let msg_sent = {
        let remaining = used.len();
        let len = buffer_for_sent_msg.len() - remaining;
        &buffer_for_sent_msg[..len]
    };
    assert_eq!(msg, msg_sent);

    let msg = include_bytes!("../btmsg-samples/invalid-msg-id").as_slice();
    let res = BtMsg::deserialize(msg).unwrap_err();
    assert!(matches!(res, BtPeerMsgDecodeError::MalformedMsg_BadMsgID));

    let msg = include_bytes!("../btmsg-samples/bitfield-10bytes-short").as_slice();
    let res = BtMsg::deserialize(msg).unwrap_err();
    assert!(matches!(res, BtPeerMsgDecodeError::IOError(_)));
}
