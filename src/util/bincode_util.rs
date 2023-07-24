use bincode::Options;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum IntEncoding {
    VarInt,
    FixInt,
}

pub fn serialize<T>(value: &T) -> Result<Vec<u8>, bincode::Error>
where
    T: ?Sized + serde::Serialize,
{
    bincode::options()
        .with_big_endian()
        .with_varint_encoding()
        .with_no_limit()
        .allow_trailing_bytes()
        .serialize(value)
}

pub fn deserialize<'a, T>(bytes: &'a [u8]) -> (Result<T, bincode::Error>, IntEncoding)
where
    T: serde::Deserialize<'a>,
{
    if let Ok(data) = bincode::options()
        .with_big_endian()
        .with_varint_encoding()
        .with_no_limit()
        .allow_trailing_bytes()
        .deserialize(bytes)
    {
        (Ok(data), IntEncoding::VarInt)
    } else {
        (
            bincode::options()
                .with_big_endian()
                .with_fixint_encoding()
                .with_no_limit()
                .allow_trailing_bytes()
                .deserialize(bytes),
            IntEncoding::FixInt,
        )
    }
}
