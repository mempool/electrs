use bincode::Options;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum IntEncoding {
    VarInt,
    FixInt,
}

pub fn serialize_big<T>(value: &T) -> Result<Vec<u8>, bincode::Error>
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

pub fn deserialize_big<'a, T>(bytes: &'a [u8]) -> Result<T, bincode::Error>
where
    T: serde::Deserialize<'a>,
{
    bincode::options()
        .with_big_endian()
        .with_varint_encoding()
        .with_no_limit()
        .allow_trailing_bytes()
        .deserialize(bytes)
}

pub fn serialize_little<T>(value: &T) -> Result<Vec<u8>, bincode::Error>
where
    T: ?Sized + serde::Serialize,
{
    bincode::options()
        .with_little_endian()
        .with_varint_encoding()
        .with_no_limit()
        .allow_trailing_bytes()
        .serialize(value)
}

pub fn deserialize_little<'a, T>(bytes: &'a [u8]) -> Result<T, bincode::Error>
where
    T: serde::Deserialize<'a>,
{
    bincode::options()
        .with_little_endian()
        .with_varint_encoding()
        .with_no_limit()
        .allow_trailing_bytes()
        .deserialize(bytes)
}

// For deserializing borked DBs
// Only use in scripts
pub fn deserialize_big_retry<'a, T>(bytes: &'a [u8]) -> (Result<T, bincode::Error>, IntEncoding)
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
