use std::{
    os::raw::{c_char, c_int, c_uchar, c_uint, c_ulong, c_ushort},
    ptr,
};

use bytes::{BufMut, BytesMut};
use flate2::raw::{gz_headerp, mz_stream, z_streamp};

pub const Z_ENOUGH_LENS: usize = 852;
pub const Z_ENOUGH_DISTS: usize = 592;
pub const Z_ENOUGH: usize = Z_ENOUGH_LENS + Z_ENOUGH_DISTS;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ZCode {
    pub op: c_uchar,
    pub bits: c_uchar,
    pub val: c_ushort,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct ZInflateState {
    pub strm: z_streamp,
    pub inflate_mode: c_uint,

    pub last: c_int,
    pub wrap: c_int,

    pub havedict: c_int,
    pub flags: c_int,

    pub dmax: c_uint,
    pub check: c_ulong,
    pub total: c_ulong,
    pub head: gz_headerp,

    pub wbits: c_uint,
    pub wsize: c_uint,
    pub whave: c_uint,
    pub wnext: c_uint,
    pub window: *mut c_char,

    pub hold: c_ulong,
    pub bits: c_uint,

    pub length: c_uint,
    pub offset: c_uint,
    pub extra: c_uint,

    pub lencode: *mut ZCode,
    pub distcode: *mut ZCode,

    pub lenbits: c_uint,
    pub distbits: c_uint,

    pub ncode: c_uint,
    pub nlen: c_uint,
    pub ndist: c_uint,
    pub have: c_uint,
    pub next: *mut ZCode,

    pub lens: [c_ushort; 320],
    pub work: [c_ushort; 288],

    pub codes: [ZCode; Z_ENOUGH],

    pub sane: c_int,
    pub back: c_int,
    pub was: c_uint,
}

pub fn write_zlib_state(buf: &mut BytesMut, stream: &mut mz_stream) {
    buf.put_u32(stream.total_in);
    buf.put_u32(stream.total_out);
    buf.put_i32(stream.data_type);
    buf.put_u32(stream.adler);

    let state = stream.state as *mut ZInflateState;
    let state_ref = unsafe { &mut *state };

    let size = std::mem::size_of::<ZInflateState>();
    let mut buffer = vec![0; size];
    unsafe {
        ptr::copy_nonoverlapping(state, buffer.as_mut_ptr() as *mut ZInflateState, 1);
    }

    println!("Offset: {}", state_ref.offset);
    println!("Size: {}", size);
    println!("Size: {}", buffer.len());

    for byte in buffer {
        buf.put_u8(byte);
    }

    let lencode_index = unsafe { state_ref.lencode.offset_from(state_ref.codes.as_ptr()) };
    let distcode_index = unsafe { state_ref.distcode.offset_from(state_ref.codes.as_ptr()) };
    let next_index = unsafe { state_ref.next.offset_from(state_ref.codes.as_ptr()) };
    println!(
        "Lencode: {}, Distcode: {}, Next: {}",
        lencode_index, distcode_index, next_index
    );

    if lencode_index > Z_ENOUGH.try_into().unwrap() {
        panic!("Can't serialize this zlib state, lencode too high!");
    }

    buf.put_u32(lencode_index as u32);
    buf.put_u32(distcode_index as u32);
    buf.put_u32(next_index as u32);

    buf.put_u32(state_ref.lenbits);
    buf.put_u32(state_ref.distbits);
}
