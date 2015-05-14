use std::borrow::Cow;
use std::default::Default;
use std::error;
use std::fmt;
use std::mem;
use std::io::{self, Read, Write};
use std::cmp::min;
use std::convert::{From, AsRef};

use num::FromPrimitive;

use deflate::{Inflater, Flush};

use crc::Crc32;
use traits::{ReadBytesExt, HasParameters, Parameter};
use types::{ColorType, Info, Transformations};
use filter::unfilter;
use chunk::{self, ChunkType, IHDR, IDAT, IEND};
use utils;

/// TODO check if these size are reasonable
const CHUNCK_BUFFER_SIZE: usize = 10*1024;
const IMAGE_BUFFER_SIZE: usize = 30*1024;

#[derive(Debug)]
enum U32Value {
    // CHUNKS
    Length,
    Type(u32),
    Crc(ChunkType)
}
    
#[derive(Debug)]
enum State {
    Signature(u8, [u8; 7]),
    U32Byte3(U32Value, u32),
    U32Byte2(U32Value, u32),
    U32Byte1(U32Value, u32),
    U32(U32Value),
    ReadChunk(ChunkType, bool),
    PartialChunk(ChunkType),
    DecodeData(ChunkType, usize),
}

#[derive(Debug)]
/// Result of the decoding process
pub enum Decoded<'a> {
    /// Nothing decoded yet
    Nothing,
    Header(u32, u32, u8, ColorType, bool),
    ChunkBegin(u32, ChunkType),
    ChunkComplete(u32, ChunkType),
    /// Decoded raw image data
    /// 
    /// The buffer is guaranteed not to span over
    /// row boundaries.
    ImageData(&'a [u8]),
    ImageEnd,
}

#[derive(Debug)]
pub enum DecodingError {
    IoError(io::Error),
    Format(Cow<'static, str>),
    InvalidSignature,
    CrcMismatch {
        /// bytes to skip to try to recover from this error
        recover: usize,
        /// Stored CRC32 value
        crc_val: u32,
        /// Calculated CRC32 sum
        crc_sum: u32,
        chunk: ChunkType
    },
    CorruptFlateStream
}

impl error::Error for DecodingError {
    fn description(&self) -> &str {
        use self::DecodingError::*;
        match *self {
            IoError(ref err) => err.description(),
            Format(ref desc) => &desc,
            InvalidSignature => "invalid signature",
            CrcMismatch { .. } => "CRC error",
            CorruptFlateStream => "compressed data stream corrupted"
        }
    }
}

impl fmt::Display for DecodingError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "{}", (self as &error::Error).description())
    }
}

impl From<io::Error> for DecodingError {
    fn from(err: io::Error) -> DecodingError {
        DecodingError::IoError(err)
    }
}

/// PNG decoder (low-level interface)
pub struct Decoder {
    state: Option<State>,
    current_chunk: (Crc32, u32, Vec<u8>),
    inflater: Inflater,
    image_data: Vec<u8>,
    row_remaining: usize,
    info: Option<Info>
}

impl Decoder {
    /// Creates a new decoder
    ///
    /// Allocates the internal buffers (40 KiB) needed for decoding the image.
    pub fn new() -> Decoder {
        Decoder {
            state: Some(State::Signature(0, [0; 7])),
            current_chunk: (Crc32::new(), 0, Vec::with_capacity(CHUNCK_BUFFER_SIZE)),
            inflater: Inflater::new(),
            image_data: vec![0; IMAGE_BUFFER_SIZE],
            row_remaining: 0,
            info: None
        }
    }
    
    /// Resets the decoder
    pub fn reset(&mut self) {
        self.state = Some(State::Signature(0, [0; 7]));
        self.current_chunk.0 = Crc32::new();
        self.current_chunk.2.clear();
        self.inflater = Inflater::new();
        self.row_remaining = 0;
        self.info = None;
    }
    
    /// Low level decoder interface.
    ///
    /// Allows to stream partial data to the encoder. Returns a tuple containing the 
    /// bytes that have been consumed from the input buffer and the latest decoding
    /// result.
    pub fn update<'a>(&'a mut self, mut buf: &[u8])
    -> Result<(usize, Decoded<'a>), DecodingError> {
        // NOTE: Do not change the function signature without double-checking the
        //       unsafe block!
        let len = buf.len();
        while buf.len() > 0 && self.state.is_some() {
            match self.next_state(buf) {
                Ok((bytes, Decoded::Nothing)) => {
                    buf = &buf[bytes..]
                }
                Ok((bytes, result)) => {
                    buf = &buf[bytes..];
                    return Ok(
                        (len-buf.len(), 
                        // This transmute just casts the lifetime away. Since Rust only 
                        // has SESE regions, this early return cannot be worked out and
                        // such that the borrow region of self includes the whole block.
                        // The explixit lifetimes in the function signature ensure that
                        // this is safe.
                        // ### NOTE
                        // To check that everything is sound, return the result without
                        // the match (e.g. `return Ok(try!(self.next_state(buf)))`). If
                        // it compiles the returned lifetime is correct.
                        unsafe { 
                            mem::transmute::<Decoded, Decoded>(result)
                        }
                    ))
                }
                Err(err) => return Err(err)
            }
        }
        Ok((len-buf.len(), Decoded::Nothing))
    }
    
    fn next_state<'a>(&'a mut self, buf: &[u8])
    -> Result<(usize, Decoded<'a>), DecodingError> {
        use self::State::*;
        
        macro_rules! goto (
            ($n:expr, $state:expr) => ({
                self.state = Some($state); 
                Ok(($n, Decoded::Nothing))
            });
            ($state:expr) => ({
                self.state = Some($state); 
                Ok((1, Decoded::Nothing))
            });
            ($n:expr, $state:expr, emit $res:expr) => ({
                self.state = Some($state); 
                Ok(($n, $res))
            });
            ($state:expr, emit $res:expr) => ({
                self.state = Some($state); 
                Ok((1, $res))
            })
        );
        
        let current_byte = buf[0];
        
        // Driver should ensure that state is never None
        let state = self.state.take().unwrap();
        //println!("{:?}", state);

        match state {
            Signature(i, mut signature) => if i < 7 {
                signature[i as usize] = current_byte;
                goto!(Signature(i+1, signature))
            } else {
                if signature == [137, 80, 78, 71, 13, 10, 26] && current_byte == 10 {
                    goto!(U32(U32Value::Length))
                } else {
                    Err(DecodingError::InvalidSignature)
                }
            },
            U32Byte3(type_, mut val) => {
                use self::U32Value::*;
                val |= current_byte as u32;
                match type_ {
                    Length => goto!(U32(Type(val))),
                    Type(length) => {
                        let type_str = [
                            (val >> 24) as u8,
                            (val >> 16) as u8,
                            (val >> 8) as u8,
                            val as u8
                        ];
                        self.current_chunk.0.reset();
                        self.current_chunk.0.update(&type_str);
                        self.current_chunk.1 = length;
                        goto!(
                            ReadChunk(type_str, true),
                            emit Decoded::ChunkBegin(length, type_str)
                        )
                    },
                    Crc(type_str) => {
                        if val == self.current_chunk.0.checksum() {
                            goto!(
                                State::U32(U32Value::Length),
                                emit if type_str == IEND {
                                    Decoded::ImageEnd
                                } else {
                                    Decoded::ChunkComplete(val, type_str)
                                }
                            )
                        } else {
                            Err(DecodingError::CrcMismatch {
                                recover: 1,
                                crc_val: val, 
                                crc_sum: self.current_chunk.0.checksum(), 
                                chunk: type_str
                            })
                        }
                    },
                }
            },
            U32Byte2(type_, val) => {
                goto!(U32Byte3(type_, val | (current_byte as u32) << 8))
            },
            U32Byte1(type_, val) => {
                goto!(U32Byte2(type_, val | (current_byte as u32) << 16))
            },
            U32(type_) => {
                goto!(U32Byte1(type_,       (current_byte as u32) << 24))
            },
            PartialChunk(type_str) => {
                match type_str {
                    IDAT => {
                        goto!(0, DecodeData(type_str, 0))
                    },
                    // Skip other chunks
                    _ => {
                        if self.current_chunk.1 == 0 { // complete chunk
                            Ok((0, try!(self.parse_chunk(type_str))))
                        } else {
                            goto!(0, ReadChunk(type_str, true), emit Decoded::Nothing)
                        }
                    }
                }
                
            },
            ReadChunk(type_str, clear) => { 
                if clear {
                    self.current_chunk.2.clear();
                }
                if self.current_chunk.1 > 0 {
                    let (ref mut crc, ref mut remaining, ref mut c_buf) = self.current_chunk;
                    let buf_avail = c_buf.capacity() - c_buf.len();
                    let bytes_avail = min(buf.len(), buf_avail);
                    let n = min(*remaining, bytes_avail as u32);
                    if buf_avail == 0 {
                        goto!(0, PartialChunk(type_str))
                    } else {
                        let buf = &buf[..n as usize];
                        crc.update(buf);
                        c_buf.push_all(buf);
                        *remaining -= n;
                        if *remaining == 0 {
                            goto!(n as usize, PartialChunk(type_str
                            ))
                        } else {
                            goto!(n as usize, ReadChunk(type_str, false))
                        }
                        
                    }
                } else {
                    goto!(0, U32(U32Value::Crc(type_str)))
                }
            }
            DecodeData(type_str, mut n) => {
                let chunk_len = self.current_chunk.2.len();
                let remaining = self.current_chunk.1;
                if self.row_remaining == 0 {
                    self.row_remaining = if let Some(ref info) = self.info {
                        info.raw_row_length()
                    } else {
                        return Err(DecodingError::Format(
                            "IHDR chunk missing".into()
                        ))
                    }
                }
                let m = min(self.image_data.len(), self.row_remaining);
                let (eof, c, data) = try!(self.inflater.inflate(
                    &self.current_chunk.2[n..],
                    &mut self.image_data[..m],
                    Flush::None
                ));
                n += c;
                self.row_remaining -= data.len();
                if eof && n != chunk_len {
                    Err(DecodingError::CorruptFlateStream)
                } else if n == chunk_len && (data.len() == 0 || remaining != 0) {
                    goto!(
                        0,
                        ReadChunk(type_str, true),
                        emit Decoded::ImageData(data)
                    )
                } else {
                    goto!(
                        0,
                        DecodeData(type_str, n),
                        emit Decoded::ImageData(data)
                    )
                }
            }
        }
    }
    
    fn parse_chunk(&mut self, type_str: [u8; 4])
    -> Result<Decoded, DecodingError> {
        self.state = Some(State::U32(U32Value::Crc(type_str)));
        let state_ptr: *mut _ = &mut self.state;
        match match type_str {
            IHDR => {
                self.parse_ihdr()
            },
            chunk::PLTE => {
                self.parse_plte()
            },
            chunk::tRNS => {
                self.parse_trns()
            }
            // Skip other and unknown chunks:
            _ => Ok(Decoded::Nothing)
        } {
            Err(err) =>{
                // Borrow of self ends here, because Decoding error does not borrow self.
                *unsafe { &mut *state_ptr } = None;
                Err(err)
            },
            ok => ok
        }
    }
    
    fn get_info_or_err(&self) -> Result<&Info, DecodingError> {
        self.info.as_ref().ok_or(DecodingError::Format(
            "IHDR chunk missing".into()
        ))
    }
    
    fn parse_plte(&mut self)
    -> Result<Decoded, DecodingError> {
        let mut vec = Vec::new();
        vec.push_all(&self.current_chunk.2);
        self.info.as_mut().map(
            |info| info.palette = Some(vec)
        );
        Ok(Decoded::Nothing)
    }
    
    fn parse_trns(&mut self)
    -> Result<Decoded, DecodingError> {
        use types::ColorType::*;
        let color_type = {
            let info = try!(self.get_info_or_err());
            info.color_type
        };
        match color_type {
            Grayscale => unimplemented!(),
            RGB => unimplemented!(),
            Indexed => {
                {
                    let _ = try!(self
                        .info.as_ref().unwrap()
                        .palette.as_ref().ok_or(DecodingError::Format(
                        "tRNS chunk occured before PLTE chunk".into()
                    )));
                };
                let mut vec = Vec::new();
                vec.push_all(&self.current_chunk.2);
                self.info.as_mut().map(
                    |info| info.trns = Some(vec)
                );
                Ok(Decoded::Nothing)
            },
            c => Err(DecodingError::Format(
                format!("tRNS chunk found for color type ({})", c as u8).into()
            ))
        }
        
    }
    
    
    fn parse_ihdr(&mut self)
    -> Result<Decoded, DecodingError> {
        // TODO: check if color/bit depths combination is valid
        let mut buf = &self.current_chunk.2[..];
        let width = try!(buf.read_be());
        let height = try!(buf.read_be());
        let bit_depth = try!(buf.read_be());
        match bit_depth {
            1 | 2 | 4 | 8 | 16 => (),
            n => return Err(DecodingError::Format(
                format!("invalid bit depth ({})", n).into()
            ))
        }
        let color_type = try!(buf.read_be());
        let color_type = match FromPrimitive::from_u8(color_type) {
            Some(color_type) => color_type,
            None => return Err(DecodingError::Format(
                format!("invalid color type ({})", color_type).into()
            ))
        };
        match try!(buf.read_be()) { // compression method
            0u8 => (),
            n => return Err(DecodingError::Format(
                format!("unknown compression method ({})", n).into()
            ))
        }
        match try!(buf.read_be()) { // filter method
            0u8 => (),
            n => return Err(DecodingError::Format(
                format!("unknown filter method ({})", n).into()
            ))
        }
        let interlaced = match try!(buf.read_be()) {
            0u8 => false,
            1 => true,
            n => return Err(DecodingError::Format(
                format!("unknown interlace method ({})", n).into()
            ))
        };
        let mut info = Info::default();

        info.width = width;
        info.height = height;
        info.bit_depth = bit_depth;
        info.color_type = color_type;
        info.interlaced = interlaced;
        self.info = Some(info);
        Ok(Decoded::Header(
            width,
            height,
            bit_depth,
            color_type,
            interlaced
        ))
    }
}
/*
pub enum InterlaceHandling {
    /// Outputs the raw rows
    RawRows,
    /// Fill missing the pixels from the existing ones
    Rectangle,
    /// Only fill the needed pixels
    Sparkle
}

impl Parameter<Reader> for InterlaceHandling {
    fn set_param(self, this: &mut Reader) {
        this.color_output = self
    }
}*/

impl<R: Read> Parameter<Reader<R>> for Transformations {
    fn set_param(self, this: &mut Reader<R>) {
        this.transform = self
    }
}

/// PNG reader (mostly high-level interface)
///
/// Provides a high level that iterates over lines or whole images.
pub struct Reader<R: Read> {
    r: R,
    d: Decoder,
    /// Read buffer
    buf: Vec<u8>,
    /// Buffer position
    pos: usize,
    /// Buffer length
    end: usize,
    bpp: usize,
    rowlen: usize,
    /// Previous raw line
    prev: Vec<u8>,
    /// Current raw line
    current: Vec<u8>,
    /// Output transformations
    transform: Transformations,
    /// Processed line
    processed: Vec<u8>
}

impl<R: Read> Reader<R> {
    /// Creates a new PNG reader
    pub fn new(r: R) -> Reader<R> {
        Reader {
            r: r,
            d: Decoder::new(),
            buf: vec![0; CHUNCK_BUFFER_SIZE],
            pos: 0,
            end: 0,
            bpp: 0,
            rowlen: 0,
            prev: Vec::new(),
            current: Vec::new(),
            transform: ::TRANSFORM_EXPAND,
            processed: Vec::new()
        }
    }
    
    /// Reads all meta data until the first IDAT chunk
    pub fn read_info(&mut self) -> Result<&Info, DecodingError> {
        use Decoded::*;
        if let Some(ref info) = self.d.info {
            Ok(info)
        } else {
            while let Some(val) = try!(self.decode_next()) {
                match val {
                    ChunkBegin(_, IDAT) => break,
                    _ => ()
                }
            }
            self.allocate_out_buf();
            let info = match self.d.info {
                Some(ref info) => info,
                None => return Err(DecodingError::Format(
                  "IHDR chunk missing".into()
                ))
            };
            self.bpp = info.bytes_per_pixel();
            self.rowlen = info.raw_row_length();
            self.prev = vec![0; self.rowlen];
            Ok(info)
        }
    }
    /// Returns the next processed row of the image
    pub fn next_row(&mut self) -> Result<Option<&[u8]>, DecodingError> {
        use types::ColorType::*;
        let transform = self.transform;
        let (color_type, bit_depth) = {
            let info = try!(self.read_info());
            (info.color_type, info.bit_depth)
        };
        if transform == ::TRANSFORM_IDENTITY {
            self.next_raw_row()
        } else {
            // swap buffer to circumvent borrow issues
            let mut buffer = mem::replace(&mut self.processed, Vec::new());
            let got_next = if let Some(row) = try!(self.next_raw_row()) {
                try!((&mut buffer[..]).write(row));
                true
            } else {
                false
            };
            // swap back
            let _ = mem::replace(&mut self.processed, buffer);
            if got_next {
                if transform.contains(::TRANSFORM_EXPAND) {
                    match color_type {
                        Indexed => {
                            self.expand_paletted()
                        }
                        Grayscale | GrayscaleAlpha if bit_depth < 8 => self.expand_gray_u8(),
                        _ => ()
                    }
                }
                Ok(Some(&self.processed))
            } else {
                Ok(None)
            }
        }
    }
    
    /// Returns the color type and the number of bits per sample
    /// of the data returned by `Reader::next_row` and Reader::frames`.
    pub fn color_type(&mut self) -> Result<(ColorType, u8), DecodingError> {
        use types::ColorType::*;
        let t = self.transform;
        let info = try!(self.read_info());
        Ok(if t == ::TRANSFORM_IDENTITY {
            (info.color_type, info.bit_depth)
        } else if t.contains(::TRANSFORM_EXPAND) {
            let has_trns = info.trns.is_some();
            // TODO 16 bits
            match info.color_type {
                Grayscale if has_trns => (GrayscaleAlpha, 8),
                RGB if has_trns => (RGBA, 8),
                Indexed if has_trns => (RGBA, 8),
                Indexed => (RGB, 8),
                ct => (ct, 8)
            }
        } else {
            (info.color_type, info.bit_depth)
        })
    }
    
    fn allocate_out_buf(&mut self) {
        use types::ColorType::*;
        let info = self.d.info.as_ref().unwrap();
        let t = self.transform;
        let trns = info.trns.is_some();
        // TODO 16 bit
        let bits = match info.color_type {
            Indexed if trns && t.contains(::TRANSFORM_EXPAND) => 4 * 8,
            Indexed if t.contains(::TRANSFORM_EXPAND) => 3 * 8,
            RGB if trns && t.contains(::TRANSFORM_EXPAND) => 4 * 8,
            Grayscale if trns && t.contains(::TRANSFORM_EXPAND) => 2 * 8,
            Grayscale if t.contains(::TRANSFORM_EXPAND) => 1 * 8,
            GrayscaleAlpha if t.contains(::TRANSFORM_EXPAND) => 2 * 8,
            t => info.bits_per_pixel()
        } * info.width as usize;
        let len = bits / 8;
        let extra = bits % 8;
        self.processed = vec![0; len + match extra { 0 => 0, _ => 1 }]
    }
    
    fn expand_gray_u8(&mut self) {
        let info = self.d.info.as_ref().unwrap();
        let samples = info.color_type.samples();
        let rescale = true;
        if let Some(ref trns) = info.trns {
            let scaling_factor = if rescale {
                (255)/((1u16 << info.bit_depth) - 1) as u8
            } else {
                1
            };
            utils::unpack_bits(&mut self.processed, 2, info.bit_depth, |pixel, chunk| {
                if pixel == trns[1] {
                    chunk[1] = 0
                } else {
                    chunk[1] = 0xFF
                }
                chunk[0] = pixel * scaling_factor
            })
        } else {
            utils::unpack_bits(&mut self.processed, 1, info.bit_depth, |val, chunk| {
                chunk[0] = val
            })
        }
    }
    
    fn expand_paletted(&mut self) {
        let transform = self.transform;
        let info = self.d.info.as_ref().unwrap();
        let palette = info.palette.as_ref().unwrap_or_else(|| panic!());
        if let Some(ref trns) = info.trns {
            utils::unpack_bits(&mut self.processed, 4, info.bit_depth, |i, chunk| {
                let (rgb, a) = (
                    // TODO prevent panic!
                    &palette[3*i as usize..3*i as usize+3],
                    *trns.get(i as usize).unwrap_or(&0xFF)
                );
                chunk[0] = rgb[0];
                chunk[1] = rgb[1];
                chunk[2] = rgb[2];
                chunk[3] = a;
            });
        } else {
            utils::unpack_bits(&mut self.processed, 3, info.bit_depth, |i, chunk| {
                let rgb = &palette[3*i as usize..3*i as usize+3];
                chunk[0] = rgb[0];
                chunk[1] = rgb[1];
                chunk[2] = rgb[2];
            })
        }
    }
    
    /// Returns the next raw row of the image
    pub fn next_raw_row(&mut self) -> Result<Option<&[u8]>, DecodingError> {
        let _ = try!(self.read_info());
        let bpp = self.bpp;
        let rowlen = self.rowlen;
        while let Some(val) = try!(decode_next(
            &mut self.r, &mut self.d, &mut self.pos,
            &mut self.end, &mut self.buf
        )) {
            match val {
                Decoded::ImageData(data) => {
                    self.current.push_all(data);
                    if self.current.len() == rowlen {
                        if let Some(filter) = FromPrimitive::from_u8(self.current[0]) {
                            unfilter(filter, bpp, &self.prev[1..], &mut self.current[1..]);
                            mem::swap(&mut self.prev, &mut self.current);
                            self.current.clear();
                            return Ok(Some(&self.prev[1..]))
                        } else {
                            return Err(DecodingError::Format(
                                format!("invalid filter method ({})", self.current[0]).into()
                            ))
                        }
                    }
                },
                _ => ()
            }
        }
        Ok(None)
    }
    
    /// Returns the next decoded block (low-level)
    pub fn decode_next(&mut self) -> Result<Option<Decoded>, DecodingError> {
        decode_next(
            &mut self.r, &mut self.d, &mut self.pos,
            &mut self.end, &mut self.buf
        )
    }
}

/// Free function form of Reader::decode_next to circumvent borrow issues
fn decode_next<'a, R: Read>(
    r: &mut R, d: &'a mut Decoder,
    pos: &mut usize, end: &mut usize, buf: &mut [u8])
-> Result<Option<Decoded<'a>>, DecodingError> {
    loop {
        if pos == end {
            *end = try!(r.read(buf));
            *pos = 0;
        }
        match try!(d.update(&buf[*pos..*end])) {
            (n, Decoded::Nothing) => *pos += n,
            (_, Decoded::ImageEnd) => return Ok(None),
            (n, result) => {
                *pos += n;
                return Ok(Some(unsafe {
                    // This transmute just casts the lifetime away. See comment
                    // in Decoder::update for more information.
                    mem::transmute::<Decoded, Decoded>(result)
                }))
            }
        }
    }
}

impl<R: Read> HasParameters for Reader<R> {}

#[test]
fn size_correct() {
    use std::fs::File;
    let files = [
        "tests/samples/PNG_transparency_demonstration_1.png",
        "tests/pngsuite/oi4n2c16.png",
    ];
    for file in files.iter() {
        let mut reader = Reader::new(File::open(file).unwrap());
        let expected_bytes = reader.read_info().unwrap().raw_bytes();
        let mut bytes = 0;
        while let Some(obj) = reader.decode_next().unwrap() {
            match obj {
                Decoded::ImageData(data) => bytes += data.len(),
                _ => ()
            }
        }
        assert_eq!(expected_bytes, bytes);
    }
}

#[test]
fn rows_ok() {
    use std::fs::File;
    let mut reader = Reader::new(File::open("tests/samples/PNG_transparency_demonstration_1.png").unwrap());
    let expected_bytes = reader.read_info().unwrap().raw_bytes();
    while let Some(row) = reader.next_row().unwrap() {
    }
}
