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
    fn a_node_record_decodes_the_properties_it_encodes(properties in properties()) {
        let record = NodeRecord { labels: BTreeSet::new(), properties };
        let decoded = NodeRecord::decode_properties(&record.encode()).unwrap();
        prop_assert_eq!(decoded.len(), record.properties.len());
        for (key, value) in &decoded {
            prop_assert!(agrees(Some(value.clone()), record.properties.get(key)));
        }
    }

    #[test]
    fn a_record_written_before_node_meta_reads_its_labels_and_properties(
        labels in prop::collection::btree_set((0u32..4).prop_map(Token::new), 0..3),
        properties in properties(),
    ) {
        let mut legacy = Vec::new();
        put_varint(&mut legacy, labels.len());
        for label in &labels {
            legacy.extend_from_slice(&label.id().to_be_bytes());
        }
        let record = NodeRecord { labels: labels.clone(), properties };
        legacy.extend_from_slice(&record.encode());
        let read = NodeRecord::decode_with_labels(&legacy).unwrap();
        prop_assert_eq!(read.labels, labels);
        prop_assert_eq!(read.properties.len(), record.properties.len());
    }

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
