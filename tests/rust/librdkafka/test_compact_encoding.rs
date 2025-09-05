use bytes::BytesMut;

// Test compact array encoding
fn test_compact_array_len() {
    // From parser.rs
    pub fn write_unsigned_varint(buf: &mut BytesMut, value: u32) {
        let mut value = value;
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            buf.extend_from_slice(&[byte]);
            if value == 0 {
                break;
            }
        }
    }
    
    pub fn write_compact_array_len(buf: &mut BytesMut, len: usize) {
        write_unsigned_varint(buf, (len + 1) as u32);
    }
    
    let mut buf = BytesMut::new();
    
    // Test encoding 1 broker
    write_compact_array_len(&mut buf, 1);
    
    println!("Encoded 1 broker as: {:02x?}", buf.as_ref());
    
    // Should be 0x02 (1 + 1)
    assert_eq!(buf.as_ref(), &[0x02]);
    
    buf.clear();
    
    // Test encoding 0 brokers
    write_compact_array_len(&mut buf, 0);
    println!("Encoded 0 brokers as: {:02x?}", buf.as_ref());
    assert_eq!(buf.as_ref(), &[0x01]); // 0 + 1
}

fn main() {
    test_compact_array_len();
    println!("All tests passed!");
}