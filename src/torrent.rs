#![allow(non_camel_case_types)]

use std::cmp::{min, Ordering};
use std::ffi::OsString;
use std::path::PathBuf;
use hex_literal::hex;
use libc::c_int;
use drizzle_bencode::BEncodeNode;
use crate::BLOCK_SIZE;

static DR_PAUL_PIECES : &[u8] = include_bytes!("../assets/dr-ron-paul-pieces").as_slice();

pub struct Torrent {
    pub info_hash: Vec<u8>,  // to be computed
    pub file_name: String,  // info.name
    pub vfile_length: u64,  // info.length OR sum(file.length for file in files)
    pub piece_length: u32,  // info."piece length" yes with a fucking space
    pub piece_hashes: Vec<u8>,
    pub vfile2name: Vec<PathBuf>,
    pub rootdir: PathBuf,

    pub downloaded_piece_set: Vec<bool>,
}

impl Torrent {
    pub fn nth_hash(&self, n: usize) -> &[u8] {
        &self.piece_hashes[n*20..n*20+20]
    }

    pub fn n_pieces(&self) -> usize {  // TODO FIX MY FUCKING INTEGER TYPES
        (self.vfile_length / self.piece_length as u64 + if self.vfile_length % self.piece_length as u64 > 0 { 1 } else { 0 }) as usize
    }

    pub fn pieces(&self) -> Vec<Piece> {
        unimplemented!()
    }

    pub fn n_files(&self) -> usize {
        self.vfile2name.len()
    }

    pub fn new_dr_paul() -> Self {
        let info_hash = hex!("ead171549d26122fe22a7ece4ab9d83a28e9679f").as_slice().to_vec();
        let file_name = "It's happening".to_string();
        let file_length = 6490669u64;
        let piece_length = 32768u32;
        let piece_hashes = DR_PAUL_PIECES.to_vec();
        let mut npieces = file_length / piece_length as u64;
        if (file_length % piece_length as u64) != 0 {
            npieces += 1;
        }

        let downloaded_piece_set = vec![false; npieces as usize];
        unimplemented!();
        // Self {
        //     info_hash,
        //     file_name,
        //     vfile_length: file_length,
        //     piece_length,
        //     piece_hashes,
        //     downloaded_piece_set
        // }
    }
}

pub struct Piece {
    // this piece_idx ...
    pub piece_idx: u32,
    pub piece_len: u32,
    pub piece_hash: [u8; 20],
    // ... is at these coordinates in the filesystem:
    pub backing_stores: Vec<BackingStore>,
}

impl Piece {
    pub fn n_blocks(&self) -> u32 {
        self.piece_len / BLOCK_SIZE + if (self.piece_len % BLOCK_SIZE) > 0 {1} else {0}
    }

    pub fn get_backingstores(&self, torrent_piece_len: u32, vfile2voffset: &VFile2VOffset, vfile2fd: &VFile2FD) -> Vec<BackingStore> {
        let mut retval = Vec::new();
        // 1 piece guaranteed because every Piece must have ONE AT LEAST? corresponding vfile.
        let mut len = self.piece_len;
        let voffset: usize = (self.piece_idx as usize * torrent_piece_len as usize);
        let first_vfile_idx = vfile2voffset.find_vfile_by_voffset(voffset);
        let first_fdoffset = voffset - vfile2voffset.get_start_voffset_of_vfile(first_vfile_idx);
        let first_voffset_end = vfile2voffset.get_end_voffset_of_vfile(first_vfile_idx);
        let first_len = min(len, (first_voffset_end - voffset) as u32);
        retval.push(BackingStore{
            fd: vfile2fd.0[first_vfile_idx].0,
            offset: first_fdoffset,
            len: first_len as usize,
        });
        len -= first_len;
        // Now let's work to exhaust that len.
        let mut next_vfile_idx = first_vfile_idx + 1;
        while len > 0 {
            let next_fdoffset = 0;
            let current_voffset_bgn = vfile2voffset.get_start_voffset_of_vfile(next_vfile_idx);
            let current_voffset_end = vfile2voffset.get_end_voffset_of_vfile(next_vfile_idx);
            let next_len = min(len, (current_voffset_end - current_voffset_bgn) as u32);
            retval.push(BackingStore{
                fd: vfile2fd.0[next_vfile_idx].0,
                offset: next_fdoffset,
                len: next_len as usize,
            });
            len -= next_len;
            next_vfile_idx += 1;
        }
        retval
    }
}

pub struct VFile2VOffset(
    pub Vec<usize>
);

impl VFile2VOffset {
    pub fn find_vfile_by_voffset(&self, offset_at_which_the_file_begins: usize) -> usize {
        match self.0.binary_search(&offset_at_which_the_file_begins) {
            Ok(i) => i,
            Err(i) => i,
        }
    }
    pub fn get_start_voffset_of_vfile(&self, vfile_idx: usize) -> usize {
        if vfile_idx == 0 { return 0; }
        self.0[vfile_idx - 1]
    }
    pub fn get_end_voffset_of_vfile(&self, vfile_idx: usize) -> usize {
        self.0[vfile_idx]
    }
}

pub struct VFile2FD(
    pub Vec<(c_int, u32)>
);

#[derive(Debug)]
pub struct BackingStore {
    pub fd: c_int,
    pub offset: usize,
    pub len: usize,
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
