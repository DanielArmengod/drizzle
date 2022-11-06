#![allow(non_camel_case_types)]
use std::ffi::OsString;
use hex_literal::hex;
use crate::bencode::BEncodeNode;

pub struct Torrent {
    pub info_hash: Vec<u8>,  // to be computed
    pub file_name: String,  // info.name
    pub file_length: u64,  // info.length
    pub piece_length: u32,  // info."piece length" yes with a fucking space
    pub piece_hashes: Vec<u8>,

    pub downloaded_piece_set: Vec<u8>,
}

static DR_PAUL_PIECES : &[u8] = include_bytes!("../assets/dr-ron-paul-pieces").as_slice();

impl Torrent {
    pub fn nth_hash(&self, n: usize) -> &[u8] {
        &self.piece_hashes[n*20..n*20+20]
    }

    pub fn new_dr_paul() -> Self {
        let info_hash = hex!("ead171549d26122fe22a7ece4ab9d83a28e9679f").as_slice().to_vec();
        let file_name = "Hello Dr. Ron Paul From Kiwifarms".to_string();
        let file_length = 6490669u64;
        let piece_length = 32768u32;
        let piece_hashes = DR_PAUL_PIECES.to_vec();
        let mut npieces = file_length / piece_length as u64;
        if (file_length % piece_length as u64) != 0 {
            npieces += 1;
        }

        let downloaded_piece_set = vec![0x00u8; npieces as usize];

        Self {
            info_hash,
            file_name,
            file_length,
            piece_length,
            piece_hashes,
            downloaded_piece_set
        }
    }
}



// This is completely fucked up, especially because `info_hash` has to be computed from the
// bencoded value form of the `info`:`<value>` dictionary. Which the current bencode.rs module has
// no API for.

// Soo we're gonna be doing it manually for a while, sorry.
// impl TryFrom<BEncodeNode> for Torrent {
//     type Error = ();
//
//     /// Consume the generic bencode-object and return a specialized Torrent struct.
//     fn try_from(value: BEncodeNode) -> Result<Self, Self::Error> {
//         let piece_hashes = if let BEncodeNode::ByteString(v) =
//         value
//             .get_dict(b"info").ok_or(())?
//             .get_dict(b"pieces").ok_or(())?
//         {
//             v.clone()
//         } else { panic!() }
//
//
//         Ok(Torrent {
//             info_hash,
//             file_name,
//             file_length,
//             piece_length,
//             piece_hashes,
//
//             downloaded_piece_set
//         })
//     }
// }
