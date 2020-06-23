use crate::zlib::Inflate;
use byteorder::{BigEndian, ByteOrder};
use filebuffer::FileBuffer;
use git_object::SHA1_SIZE;
use quick_error::quick_error;
use std::convert::TryInto;
use std::{convert::TryFrom, mem::size_of, path::Path};

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: std::io::Error, path: std::path::PathBuf) {
            display("Could not open pack file at '{}'", path.display())
            cause(err)
        }
        Corrupt(msg: String) {
            display("{}", msg)
        }
        UnsupportedVersion(version: u32) {
            display("Unsupported pack version: {}", version)
        }
        ZlibInflate(err: crate::zlib::Error, msg: &'static str) {
            display("{}", msg)
            cause(err)
        }
    }
}

const N32_SIZE: usize = size_of::<u32>();

#[derive(PartialEq, Eq, Debug, Hash, Ord, PartialOrd, Clone)]
pub enum Kind {
    V2,
    V3,
}

#[derive(PartialEq, Eq, Debug, Hash, Ord, PartialOrd, Clone)]
pub struct Entry {
    pub header: decoded::Header,
    /// The decompressed size of the object in bytes
    pub size: u64,
    /// absolute offset to compressed object data in the pack
    pub offset: u64,
}

pub struct File {
    data: FileBuffer,
    kind: Kind,
    num_objects: u32,
}

impl File {
    pub fn kind(&self) -> Kind {
        self.kind.clone()
    }
    pub fn num_objects(&self) -> u32 {
        self.num_objects
    }

    fn assure_v2(&self) {
        assert!(
            if let Kind::V2 = self.kind.clone() {
                true
            } else {
                false
            },
            "Only V2 is implemented"
        );
    }

    pub fn entry(&self, offset: u64) -> Entry {
        self.assure_v2();
        let pack_offset: usize = offset.try_into().expect("offset representable by machine");
        assert!(pack_offset <= self.data.len(), "offset out of bounds");

        let object_data = &self.data[pack_offset..];
        let (object, decompressed_size, consumed_bytes) =
            decoded::Header::from_bytes(object_data, offset);
        Entry {
            header: object,
            size: decompressed_size,
            offset: offset + consumed_bytes,
        }
    }

    pub fn at(path: impl AsRef<Path>) -> Result<File, Error> {
        File::try_from(path.as_ref())
    }

    pub fn decode_entry(&self, entry: &Entry, out: &mut [u8]) -> Result<(), Error> {
        use crate::pack::decoded::Header::*;
        assert!(
            out.len() as u64 >= entry.size,
            "output buffer isn't large enough to hold decompressed result, want {}, have {}",
            entry.size,
            out.len()
        );
        let offset: usize = entry
            .offset
            .try_into()
            .expect("offset representable by machine");
        assert!(offset <= self.data.len(), "entry offset out of bounds");

        match entry.header {
            Commit | Tree | Blob | Tag => Inflate::default()
                .once(&self.data[offset..], &mut std::io::Cursor::new(out), true)
                .map_err(|e| Error::ZlibInflate(e, "Failed to decompress pack entry"))
                .map(|_| ()),
            OfsDelta { pack_offset } => {
                unimplemented!("{:#b} {:#?}, {:#?}", 127, entry, self.entry(pack_offset))
            }
            RefDelta { .. } => unimplemented!("ref delta"),
        }
    }
}

impl TryFrom<&Path> for File {
    type Error = Error;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        let data = FileBuffer::open(path).map_err(|e| Error::Io(e, path.to_owned()))?;
        let pack_len = data.len();
        if pack_len < N32_SIZE * 3 + SHA1_SIZE {
            return Err(Error::Corrupt(format!(
                "Pack file of size {} is too small for even an empty pack",
                pack_len
            )));
        }
        let mut ofs = 0;
        if &data[ofs..ofs + b"PACK".len()] != b"PACK" {
            return Err(Error::Corrupt("Pack file type not recognized".into()));
        }
        ofs += N32_SIZE;
        let kind = match BigEndian::read_u32(&data[ofs..ofs + N32_SIZE]) {
            2 => Kind::V2,
            3 => Kind::V3,
            v => return Err(Error::UnsupportedVersion(v)),
        };
        ofs += N32_SIZE;
        let num_objects = BigEndian::read_u32(&data[ofs..ofs + N32_SIZE]);

        Ok(File {
            data,
            kind,
            num_objects,
        })
    }
}

pub mod decoded;