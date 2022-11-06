#[macro_use]
extern crate num_derive;
extern crate core;

mod btmsg;
mod bencode;
mod torrent;

use std::cmp::{max, min, Ordering};
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Cursor, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use hex_literal::hex;
use itertools::Itertools;
use libc::{c_int, c_void, EFD_SEMAPHORE, epoll_create, epoll_ctl, EPOLL_CTL_ADD, epoll_event, epoll_wait, EPOLLIN, eventfd, off_t, read, size_t, ssize_t, write};
use crate::torrent::Torrent;
use sha1_smol::Sha1;
use crate::btmsg::{BtMsg, recv_and_check_handshake, send_handshake};

enum TM2PMCmd {
    DownloadPiece(Piece),
}

enum PM2TMResult {
    FinishedDownloadingPiece(u32)
}

struct Piece {
    // this piece_idx ...
    piece_idx: u32,
    piece_len: u32,
    piece_hash: [u8; 20],
    // ... is at these coordinates in the filesystem:
    fd: c_int,
    offset: usize,
}

impl Piece {
    fn n_blocks(&self) -> u32 {
        self.piece_len / BLOCK_SIZE + if (self.piece_len % BLOCK_SIZE) > 0 {1} else {0}
    }
}

struct BlockReq {
    begin: u32,
    length: u32
}

impl BlockReq {
    fn block_idx(&self) -> u32 { self.begin / BLOCK_SIZE }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BlockResp {
    begin: u32,
    data: Vec<u8>  // .length is .data.len()
}

impl BlockResp {
    fn block_idx(&self) -> u32 { self.begin / BLOCK_SIZE }
}

struct PieceInFlight {
    piece: Piece,
    ready_blks: Vec<BlockResp>,
    unasked_blks: Vec<BlockReq>,
}

static BLOCK_SIZE : u32 = 0x4000;

impl PieceInFlight {
    // This structure is a bit wonky. Explained:
    // Each piece in flight is represented by three elements:
    // * The piece itself, which, once fully received, has all necessary information to check itself and write itself to disk.
    // * A list of BlockResp's representing blocks that have been received from the peer. Those objects OWN their buffers.
    // * A list of BlockReq's that we have yet to ask for.
    // Operations that can be performed on a list (also wonky) of this (wonky) data structure.
    // * Add a piece: easy, just shove it at the end.
    // * Find a piece by index: walk the entire list, match on elt.piece.piece_idx, possibly return &mut elt. Complexity class: GoFuckYourself*(log(n))

    // What happens when we recv a BlockResponse?
    // -> Extract piece_idx from BtMsg::BlockResponse
    // -> Find PieceInFlight within the list or set or whatever -> Piece
    //   probably `
    // ->
    //
    fn new(piece: Piece) -> Self {
        let n_blocks = piece.n_blocks();
        let ready_blks = Vec::with_capacity(n_blocks as usize);
        let mut unasked_blks = Vec::with_capacity(n_blocks as usize);
        for i in 0..n_blocks {
            let begin = BLOCK_SIZE * i;
            let length = min(piece.piece_len - begin, BLOCK_SIZE);
            unasked_blks.push(BlockReq {
                begin,
                length
            });
        }
        Self { piece, ready_blks, unasked_blks }
    }

    pub fn unasked_to_asked(&mut self, block_no: u32) {
        // Moving a block from state: "unasked" to state: "asked but not yet received" is as simple as removing it from its list. We "forget" about it, but it will surely (if the network protocol is reliable) come back to us as a BtMsg::BlockResponse.
        // TODO This function automatically panics if no block with block_no number is in the unasked_blks list.
        // TODO: assert that the block is neither in unasked_blks nor in ready_blks
        self.unasked_blks.swap_remove(
            self.unasked_blks.iter().position(|elt| elt.block_idx() == block_no).unwrap()
        );
    }

    pub fn pending_to_received(&mut self, resp: BlockResp) {
        // TODO: assert that the block is neither in unasked_blks nor in ready_blks
        self.ready_blks.push(resp);
    }
    
    pub fn is_completely_downloaded(&self) -> bool {
        self.piece.n_blocks() as usize == self.ready_blks.len()
    }
}

enum PMEvent {
    TorrentMasterCmd,
    PeerSocketReady4Read,
}

static MAX_BLOCKS_IN_FLIGHT : u32 = 10;

fn next_requestable_block(unstarted_piece_queue: &mut VecDeque<Piece>, pieces_in_flight: &mut Vec<PieceInFlight>) -> Option<(u32, u32, u32)> {
    /// Looks for the next block that can be requested.
    /// If there exists any piece "in flight", that is, for which we have already asked the peer some blocks, AND this piece has AT LEAST ONE block which we haven't requested yet, return a triplet specifying (piece_idx, begin, len).
    /// If no piece "in fight" has any blocks we have yet to ask from the peer, then see if there are any pieces we haven't even started working on. Take the first, move it to the "in flight" set.
    ///
    /// ATTENTION: modifies `unstarted_piece_queue` and `pieces_in_flight` as needed.
    if let Some(piece_with_unasked_blks) = pieces_in_flight.iter_mut().find(|piece| ! piece.unasked_blks.is_empty()) {
        let blk = piece_with_unasked_blks.unasked_blks.pop().unwrap();
        return Some((piece_with_unasked_blks.piece.piece_idx, blk.begin, blk.length));
    }
    if let Some(piece) = unstarted_piece_queue.pop_front() {
        let mut unasked_blks = Vec::with_capacity((piece.n_blocks() - 1) as usize);
        let piece_to_return = Some({
            let blkstart = 0;
            let blksize = min(piece.piece_len - blkstart, BLOCK_SIZE);
            (piece.piece_idx, blkstart, blksize)
        });
        for i in 1..piece.n_blocks() {
            let blkstart = BLOCK_SIZE * i;
            let blksize = min(piece.piece_len - blkstart, BLOCK_SIZE);
            unasked_blks.push(BlockReq {
                begin: blkstart,
                length: blksize
            });
        }
        pieces_in_flight.push(PieceInFlight {
            piece,
            ready_blks: Vec::new(),
            unasked_blks
        });
        return piece_to_return;
    }
    None
}

// fn torrent_master(torrent: Torrent) {
//     let mut peers_unconn = Vec::<Peer>::new();
//     // FAKE acquire peers
//     peers_conn.push(Some(Peer::new("localhost:6969")));
//     peers_conn.push(Some(Peer::new("localhost:6970")));
//
// }

fn get_next_event(epoll_instance: c_int, sock_fd: c_int, eventfd_fd: c_int) -> PMEvent { unsafe {
    eprintln!("Entered GET_NEXT_EVENT.");
    let mut event = epoll_event{events: 0, u64: 0};
    let r = epoll_wait(epoll_instance, &mut event, 1, -1);
    assert_ne!(r, -1);
    eprintln!("GET_NEXT_EVENT ............. DONE.");
    let FUCKYOU = event.u64;
    if FUCKYOU == sock_fd as u64 {
        return PMEvent::PeerSocketReady4Read;
    }
    if FUCKYOU == eventfd_fd as u64 {
        let mut myu64: u64 = 0;
        assert_eq!(read(eventfd_fd, &mut myu64 as *mut _ as _, 8), 8);
        assert_eq!(myu64, 1);
        return PMEvent::TorrentMasterCmd;
    }
    panic!();
}}

fn peer_master(mut sock: TcpStream, mut orders_in: Receiver<TM2PMCmd>, orders_in__eventfd: c_int, mut results_out: Sender<PM2TMResult>) {
    // ↓↓↓ STATE ↓↓↓
    let socket_fd = sock.as_raw_fd();
    let epoll_fd: c_int;
    let mut piece_download_queue: VecDeque<Piece> = VecDeque::new();
    let mut dl_pieces_in_flight: Vec<PieceInFlight> = Vec::new();
    let mut blocks_in_flight = 0;

    let (mut am_choked, mut peer_is_choked, mut am_interested, mut peer_is_interested) = (true, true, false, false);
    // ↑↑↑ END STATE ↑↑↑

    // ↓↓↓ SETUP ↓↓↓
    // Handshake is already done by Torrent master, who opens the connection to a potential peer, handshakes, and then hands it off to us.
    // We should be responsible for asking the peer what pieces it has, forwarding that information to the TM, who then assigns us a bunch of pieces to download.
    // For now, we don't have to do any of that. We just - BLOCKINGLY - tell the peer we're interested, and then enter the main loop. We'll receive the "unchoke" message there (not that we care much at this point).
    BtMsg::Interested.serialize(&mut sock).unwrap();
    loop {
        match BtMsg::deserialize(&mut sock) {
            Ok(BtMsg::Unchoke) => break,
            m => eprintln!("Received unexpected btmsg: {:?}", m),
        }
    }

    // Set up the eventfd instance for this peer manager: we care about the socket with our peer and the command channel with our master.
    unsafe {
        epoll_fd = epoll_create(10); assert!(epoll_fd > 0);
        // Add the socket to this epoll instance's interest list.
        assert_eq!(
            epoll_ctl(epoll_fd, EPOLL_CTL_ADD, socket_fd, &mut epoll_event{events: EPOLLIN as u32, u64: socket_fd as u64 }),
        0);
        // Add the eventfd instance married to the MPSC queue to this epoll instance's interest list.
        assert_eq!(
            epoll_ctl(epoll_fd, EPOLL_CTL_ADD, orders_in__eventfd, &mut epoll_event{events: EPOLLIN as u32, u64: orders_in__eventfd as u64 }),
        0);
    }
    // ↑↑↑ END SETUP ↑↑↑

    // ↓↓↓ MAIN LOOP ↓↓↓
    loop {
        let next_event = get_next_event(epoll_fd, socket_fd, orders_in__eventfd);
        match next_event {
            PMEvent::TorrentMasterCmd => {
                let order = orders_in.try_recv().unwrap();
                match order {
                    TM2PMCmd::DownloadPiece(piece) => {
                        if blocks_in_flight < MAX_BLOCKS_IN_FLIGHT {
                            // The only way this can be the case is that
                            let blocks_to_a_piece = piece.n_blocks();
                            let blocks_we_can_ask_for = min(MAX_BLOCKS_IN_FLIGHT - blocks_in_flight, blocks_to_a_piece);
                            for i in 0..blocks_we_can_ask_for {
                                let blkstart = i * BLOCK_SIZE;
                                // dbg!(piece.piece_len);
                                // dbg!(blkstart);
                                let blksize = min(piece.piece_len - blkstart, BLOCK_SIZE);
                                // BEGIN ask_for_block(idx : piece.piece_idx, start: blkstart, len: blksize );
                                BtMsg::BlockRequest {
                                    piece_idx: piece.piece_idx,
                                    begin: blkstart,
                                    length: blksize
                                }.serialize(&mut sock).unwrap();
                                //  END  ask_for_block(idx : piece.piece_idx, start: blkstart, len: blksize );
                                blocks_in_flight += 1;
                            }
                            let mut unasked_blks = Vec::with_capacity((blocks_to_a_piece - blocks_we_can_ask_for) as usize);
                            for i in blocks_we_can_ask_for..blocks_to_a_piece {
                                let blkstart = i * BLOCK_SIZE;
                                let blksize = min(piece.piece_len - blkstart, BLOCK_SIZE);
                                unasked_blks.push(BlockReq {
                                    begin: blkstart,
                                    length: blksize
                                })
                            }
                            dl_pieces_in_flight.push(PieceInFlight {
                                piece,
                                ready_blks: Vec::new(),
                                unasked_blks
                            })
                        } else {
                            piece_download_queue.push_back(piece);
                        }
                    }
                    // NO OTHER "TM2PM" COMMANDS YET.
                }
            }
            PMEvent::PeerSocketReady4Read => {
                let btmsg : BtMsg = BtMsg::deserialize(&mut sock).unwrap();  // TODO: This will actually block; BtMsg::deserialize should be somehow made async, and polled once every time eventfd/epoll indicates that the socket is ready for reading?
                match btmsg {
                    BtMsg::BlockResponse {piece_idx, begin, data} => {
                        let piece_in_flight_pos = dl_pieces_in_flight.iter_mut().position(|elt| elt.piece.piece_idx == piece_idx).unwrap();
                        let piece_in_flight = &mut dl_pieces_in_flight[piece_in_flight_pos];
                        piece_in_flight.pending_to_received(BlockResp { begin, data });

                        // If the piece is complete, we'll need to verify it and flush it to disk.
                        if piece_in_flight.is_completely_downloaded() {
                            piece_in_flight.ready_blks.sort();
                            // Verify
                            let published_hash = piece_in_flight.piece.piece_hash;
                            let mut hasher = Sha1::new();
                            for block in &piece_in_flight.ready_blks {
                                hasher.update(&block.data);
                            }
                            let computed_hash = hasher.digest().bytes();
                            assert_eq!(published_hash, computed_hash);  // TODO Very robust error-handling. Lmao.
                            // Write to disk
                            let fd = piece_in_flight.piece.fd;
                            let base_offset = piece_in_flight.piece.offset;
                            for block in &piece_in_flight.ready_blks {
                                let res = pwrite(fd, &block.data, (base_offset + block.begin as usize) as off_t);
                                assert_ne!(res, -1);   // TODO Very robust error-handling. Lmao.
                            }
                            // Mark the piece as complete. Also, inform the Torrent Master.
                            results_out.send(PM2TMResult::FinishedDownloadingPiece(piece_idx)).unwrap();
                            dl_pieces_in_flight.swap_remove(piece_in_flight_pos);  // This will drop all buffers for this piece.
                        }

                        if let Some((piece_idx, begin, length)) = next_requestable_block(&mut piece_download_queue, &mut dl_pieces_in_flight) {
                            // BEGIN ask_for_block(idx : piece.piece_idx, start: blkstart, len: blksize );
                            BtMsg::BlockRequest {
                                piece_idx,
                                begin,
                                length,
                            }.serialize(&mut sock).unwrap();
                            //  END  ask_for_block(idx : piece.piece_idx, start: blkstart, len: blksize );
                        } else {
                            blocks_in_flight -= 1;
                        }
                    }
                    BtMsg::Unchoke => { am_choked = false; eprintln!("PM unchoked."); }
                    BtMsg::Choke => { am_choked = true; panic!(); } // We will have a public metldown if we're refused.
                    BtMsg::Interested => peer_is_interested = true,
                    BtMsg::Uninterested => peer_is_interested = false,
                    BtMsg::BlockRequest {..} => unimplemented!(),  // We shouldn't receive this, because all our peers will be seeds for now.
                    BtMsg::HavePiece {..} => unimplemented!(),  // We shouldn't receive this, because all our peers will be seeds for now.
                    BtMsg::Bitfield {..} => {}   // We don't care about receiving this, because we already assume every peer is a full seed.
                    BtMsg::KeepAlive => {},
                    BtMsg::CancelBlockRequest {..} => unimplemented!(),  // We will have a public metldown if we're refused.
                    // Others BtMsg kinds...
                }
            }
        }
    }
}


fn main ()  { unsafe {
    let mut s = TcpStream::connect("127.0.0.1:6969").unwrap();
    let mut destfile = OpenOptions::new().write(true).truncate(true).create(true).open("/tmp/plswork").unwrap();
    let mut pieces = Vec::new();
    let torrent = Torrent::new_dr_paul();

    for i in 0..199 {
        pieces.push(Piece{
            piece_idx: i,
            piece_len: min(torrent.piece_length, (torrent.file_length - (i * torrent.piece_length) as u64) as u32),
            offset: (i * torrent.piece_length) as usize,
            fd: destfile.as_raw_fd(),
            piece_hash: torrent.nth_hash(i as usize).try_into().unwrap()
        })
    }

    send_handshake(&torrent.info_hash, &mut s).unwrap();
    recv_and_check_handshake(&torrent.info_hash, None, &mut s).unwrap();

    let (tm2pm_send, tm2pm_recv) = channel();
    let (pm2tm_send, pm2tm_recv) = channel();
    let orders_eventfd : libc::c_int = eventfd(0, EFD_SEMAPHORE); assert_ne!(orders_eventfd, -1);

    thread::spawn(move || peer_master(s, tm2pm_recv, orders_eventfd, pm2tm_send));

    for piece in pieces {
        tm2pm_send.send(TM2PMCmd::DownloadPiece(piece)).unwrap();
        assert_eq!(write(orders_eventfd, &(1 as u64) as *const _ as _, 8), 8);
    }
    for response in pm2tm_recv {
        if let PM2TMResult::FinishedDownloadingPiece(piece) = response {
            eprintln!("Main: Piece completion notification {}.", piece)
        }
    }
    eprintln!("Main finishing.");
    return ();
}}

pub fn pread(fd: RawFd, buf: &mut [u8], offset: off_t) -> ssize_t {
    unsafe {
        libc::pread(fd, buf.as_mut_ptr() as *mut c_void, buf.len() as size_t, offset)
    }
}

pub fn pwrite(fd: RawFd, buf: &[u8], offset: off_t) -> ssize_t {
    unsafe {
        libc::pwrite(fd, buf.as_ptr() as *const c_void, buf.len() as size_t, offset)
    }
}

// CUTTING FLOOR

// let mut recv_buf  : Vec<u8> = Vec::new();  // VARIABLE IS LOGICALLY UNINITIALIZED!
// // The following variable is interpreted as follows:
// // if non-zero, we were reading from the socket, determined we had to read N bytes, and need to pull `val` more bytes to finish.
// // if zero, we are reading the first bytes of a new transaction
// let mut read_remaining : usize = 0;
// ......................wordswordswords......................
// // HANDLING THIS EVENT CAN PRODUCE 0 OR 1 BTMSGS THAT WE HAVE TO HANDLE.
//
// // Are we picking up from assembling a BtMsg from various frames, or are we starting anew?
// if read_remaining == 0 {
//     // We are starting anew.
//     let msglen : u32 = {
//         let mut buf = [0u8;4];
//         sock.read_exact(buf.as_mut_slice()).unwrap(); // sock assumed to be nonblocking and if we don't have 4 bytes ready to read, fuck off.
//         u32::from_be_bytes(buf)
//     };
//     if msglen == 0 {
//         btmsg = BtMsg::KeepAlive;
//         read_remaining = 0;  // Unnecessary; for clarity.
//         continue;
//     } else {
//         recv_buf = vec![0u8; msglen as usize];
//         read_remaining = msglen as usize;
//         let read_so_far = recv_buf.len() - read_remaining;
//         let nread = sock.read(&mut recv_buf[read_so_far..]).unwrap();
//         read_remaining -= nread;
//     }
// } else {
//     let read_so_far = recv_buf.len() - read_remaining;
//     let nread = sock.read(&mut recv_buf[read_so_far..]).unwrap();
//     read_remaining -= nread;
// }
//
// // We've definitely done some work receiving a BtMsg; read_remaining == 0 now definitely indicates a complete packet is ready in recv_buf.
// if read_remaining == 0 {


//    let mut piece = 0;
//     loop {
//         if piece == 198 { break; }
//
//         BtMsg::BlockRequest {
//             piece_idx: piece,
//             begin: 0,
//             length: 0x4000,
//         }.serialize(&mut s).unwrap();
//         BtMsg::BlockRequest {
//             piece_idx: piece,
//             begin: 0x4000,
//             length: 0x4000,
//         }.serialize(&mut s).unwrap();
//         piece += 1;
//
//         let msg_recvd = BtMsg::deserialize(&mut s).unwrap();
//         match &msg_recvd {
//             BtMsg::BlockResponse {piece_idx, begin, data} => {
//                 let start = piece_idx * 0x8000 + begin;
//                 let end = start + 0x4000;
//                 (&mut buf4file[start as _..end as _]).copy_from_slice(data.as_slice());
//                 if *begin == 0x4000 { ask_for_more = true; }
//             },
//             _ => {}
//         }
//         let msg_recvd = BtMsg::deserialize(&mut s).unwrap();
//         match &msg_recvd {
//             BtMsg::BlockResponse {piece_idx, begin, data} => {
//                 let start = piece_idx * 0x8000 + begin;
//                 let end = start + 0x4000;
//                 (&mut buf4file[start as _..end as _]).copy_from_slice(data.as_slice());
//                 if *begin == 0x4000 { ask_for_more = true; }
//             },
//             _ => {}
//         }
//     }
//     // Last piece
//     BtMsg::BlockRequest {
//         piece_idx: 198,
//         begin: 0,
//         length: 2605,
//     }.serialize(&mut s).unwrap();
//     if let BtMsg::BlockResponse {piece_idx, begin, data} = BtMsg::deserialize(&mut s).unwrap() {
//         let start = 198 * 0x8000;
//         let end = start + 2605;
//         (&mut buf4file[start as _..end as _]).copy_from_slice(data.as_slice());
//     }