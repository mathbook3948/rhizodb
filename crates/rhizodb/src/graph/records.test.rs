use super::*;
use crate::value::tests::property_value;
use crate::value::tests::same_bits;
use proptest::prelude::*;

fn properties() -> impl Strategy<Value = Properties> {
    prop::collection::btree_map((0u32..8).prop_map(Token::new), property_value(), 0..6)
}

fn agrees(read: Option<PropertyValue>, stored: Option<&PropertyValue>) -> bool {
    match (read, stored) {
        (Some(read), Some(stored)) => same_bits(&read, stored),
        (None, None) => true,
        _ => false,
    }
}

proptest! {
    #[test]
    fn one_relationship_property_reads_what_the_whole_record_holds(
        properties in properties(),
        key in (0u32..9).prop_map(Token::new),
    ) {
        let record = RelationshipRecord {
            rel_type: Token::new(3),
            source: NodeId(7),
            target: NodeId(u64::MAX),
            properties,
        };
        let read = RelationshipRecord::property(&record.encode(), key).unwrap();
        prop_assert!(agrees(read, record.properties.get(&key)));
    }
}

fn relationship() -> impl Strategy<Value = RelationshipRecord> {
    (any::<u32>(), any::<u64>(), any::<u64>(), properties()).prop_map(
        |(rel_type, source, target, properties)| RelationshipRecord {
            rel_type: Token::new(rel_type),
            source: NodeId(source),
            target: NodeId(target),
            properties,
        },
    )
}

fn same_record(read: &RelationshipRecord, record: &RelationshipRecord) -> bool {
    read.rel_type == record.rel_type
        && read.source == record.source
        && read.target == record.target
        && read.properties.len() == record.properties.len()
        && read
            .properties
            .iter()
            .all(|(key, value)| agrees(Some(value.clone()), record.properties.get(key)))
}

proptest! {
    #[test]
    fn a_relationship_record_decodes_what_it_encodes(record in relationship()) {
        let read = RelationshipRecord::decode(&record.encode()).unwrap();
        prop_assert!(same_record(&read, &record));
    }

}

#[test]
fn a_small_relationship_record_takes_a_byte_for_each_small_number() {
    let record = RelationshipRecord {
        rel_type: Token::new(1),
        source: NodeId(100),
        target: NodeId(20_000),
        properties: Properties::from([(Token::new(2), PropertyValue::Integer(7))]),
    };
    assert_eq!(record.encode().len(), 1 + 1 + 3 + 1 + 1 + 2);
}

#[test]
fn a_relationship_type_past_the_token_range_is_refused() {
    let mut bytes = Vec::new();
    crate::value::put_varint_u64(&mut bytes, u64::from(u32::MAX) + 1);
    bytes.extend_from_slice(&[0, 0, 0]);
    assert!(RelationshipRecord::decode(&bytes).is_err());
    let mut trailing = RelationshipRecord {
        rel_type: Token::new(0),
        source: NodeId(0),
        target: NodeId(0),
        properties: Properties::new(),
    }
    .encode();
    trailing.push(0);
    assert!(RelationshipRecord::decode(&trailing).is_err());
}
