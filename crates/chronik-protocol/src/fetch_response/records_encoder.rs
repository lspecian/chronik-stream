//! Records and Aborted Transactions Encoding
//!
//! Handles encoding of record batches and aborted transactions in Fetch responses.
//! Complexity: < 20 per function

use crate::parser::Encoder;
use crate::types::AbortedTransaction;
use tracing::{trace, debug};

/// Encoder for records and transaction metadata
pub struct RecordsEncoder;

impl RecordsEncoder {
    /// Encode records bytes with flexible/compact encoding support
    ///
    /// Complexity: < 15 (branching on flexible and empty records)
    pub fn encode_records(
        encoder: &mut Encoder,
        records: &[u8],
        flexible: bool,
    ) {
        let records_len = records.len();

        if records_len == 0 {
            // Empty records
            trace!("        Records: empty (0 bytes)");
            if flexible {
                // CRITICAL FIX: Compact encoding - 1 = empty (0 bytes), NOT 0 (which is null)
                encoder.write_unsigned_varint(1); // Empty array (0 bytes)
            } else {
                encoder.write_i32(0); // Empty byte array
            }
        } else {
            // Non-empty records
            debug!("        Records: {} bytes", records_len);
            if flexible {
                encoder.write_unsigned_varint((records_len + 1) as u32);
            } else {
                encoder.write_i32(records_len as i32);
            }
            encoder.write_raw_bytes(records);
        }
    }

    /// Encode aborted transactions array (v4+)
    ///
    /// Complexity: < 20 (loop with flexible encoding)
    pub fn encode_aborted_transactions(
        encoder: &mut Encoder,
        aborted: Option<&Vec<AbortedTransaction>>,
        version: i16,
        flexible: bool,
    ) {
        if version < 4 {
            return; // Aborted transactions not supported before v4
        }

        if let Some(transactions) = aborted {
            // Encode array length
            if flexible {
                encoder.write_unsigned_varint((transactions.len() + 1) as u32);
            } else {
                encoder.write_i32(transactions.len() as i32);
            }

            // Encode each transaction
            for txn in transactions {
                encoder.write_i64(txn.producer_id);
                encoder.write_i64(txn.first_offset);

                // Tagged fields for transaction (flexible versions)
                if flexible {
                    encoder.write_unsigned_varint(0);
                }
            }
        } else {
            // No aborted transactions
            if flexible {
                encoder.write_unsigned_varint(1); // 0 + 1 for compact encoding
            } else {
                encoder.write_i32(0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn test_encode_empty_records_non_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        RecordsEncoder::encode_records(&mut encoder, &[], false);

        // Empty records: 4-byte i32 with value 0
        assert_eq!(buf.len(), 4);
        assert_eq!(&buf[..], &[0, 0, 0, 0]);
    }

    #[test]
    fn test_encode_empty_records_flexible() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        RecordsEncoder::encode_records(&mut encoder, &[], true);

        // Empty records: varint 1 (representing 0 bytes)
        assert_eq!(buf.len(), 1);
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn test_encode_non_empty_records() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        let records = vec![1, 2, 3, 4, 5];
        RecordsEncoder::encode_records(&mut encoder, &records, false);

        // Length (4 bytes) + records (5 bytes) = 9 bytes
        assert_eq!(buf.len(), 9);
    }

    #[test]
    fn test_encode_no_aborted_transactions_v3() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        RecordsEncoder::encode_aborted_transactions(&mut encoder, None, 3, false);

        // v3 doesn't support aborted transactions
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn test_encode_no_aborted_transactions_v4() {
        let mut buf = BytesMut::new();
        let mut encoder = Encoder::new(&mut buf);

        RecordsEncoder::encode_aborted_transactions(&mut encoder, None, 4, false);

        // v4+ encodes empty array as i32(0) = 4 bytes
        assert_eq!(buf.len(), 4);
    }
}
