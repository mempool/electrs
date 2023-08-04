/*

The tests below show us the following defaults for each method of using bincode.

1. Using bincode::[de]serialize() directly: "function"
2. Using bincode::config().[de]serialize(): "Config" (deprecated)
3. Using bincode::options().[de]serialize(): "Options" (currently recommended for v1.3.3)

```
+----------+--------+------------+----------------+------------+
|          | Endian | Int Length | Allow Trailing | Byte Limit |
+----------+--------+------------+----------------+------------+
| function | little | fixed      | allow          | unlimited  |
| Config   | little | fixed      | allow          | unlimited  |
| Options  | little | variable * | reject *       | unlimited  |
+----------+--------+------------+----------------+------------+
```

Thus we only need to change the int length from variable to fixed,
and allow trailing to allow in order to match the previous behavior.
(note: TxHistory was using Big Endian by explicitly setting it to big.)

*/

use bincode_do_not_use_directly as bincode;

#[test]
fn bincode_settings() {
    let value = TestStruct::new();
    let mut large = [0_u8; 4096];
    let decoded = [
        8_u8, 7, 6, 5, 4, 3, 2, 1, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1, 1,
        2, 3, 4, 5, 6, 7, 8, 12, 0, 0, 0, 0, 0, 0, 0, 72, 101, 108, 108, 111, 32, 87, 111, 114,
        108, 100, 33,
    ];
    large[0..56].copy_from_slice(&decoded);

    // Using functions: Little endian, Fixint, Allow trailing, Unlimited
    assert_eq!(bincode::serialize(&value).unwrap(), &decoded);
    assert_eq!(bincode::deserialize::<TestStruct>(&large).unwrap(), value);

    // Using Config (deprecated)
    // Little endian, fixint, Allow trailing, Unlimited
    #[allow(deprecated)]
    {
        assert_eq!(bincode::config().serialize(&value).unwrap(), &decoded);
        assert_eq!(
            bincode::config().deserialize::<TestStruct>(&large).unwrap(),
            value
        );
    }

    // Using Options
    // Little endian, VARINT (different), Reject trailing (different), unlimited
    use bincode::Options;
    assert_eq!(
        bincode::options()
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .serialize(&value)
            .unwrap(),
        &decoded
    );
    assert_eq!(
        bincode::options()
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .deserialize::<TestStruct>(&large)
            .unwrap(),
        value
    );
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TestStruct {
    a: u64,
    b: [u8; 8],
    c: TestData,
    d: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
enum TestData {
    Foo(FooStruct),
    Bar(BarStruct),
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct FooStruct {
    a: u64,
    b: [u8; 8],
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct BarStruct {
    a: u64,
    b: [u8; 8],
}

impl TestStruct {
    fn new() -> Self {
        Self {
            a: 0x0102030405060708,
            b: [1, 2, 3, 4, 5, 6, 7, 8],
            c: TestData::Foo(FooStruct {
                a: 0x0102030405060708,
                b: [1, 2, 3, 4, 5, 6, 7, 8],
            }),
            d: String::from("Hello World!"),
        }
    }
}
