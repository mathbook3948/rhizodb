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
    fn an_encoded_node_reads_the_labels_and_each_property_its_record_holds(
        labels in prop::collection::btree_set((0u32..4).prop_map(Token::new), 0..3),
        properties in properties(),
        key in (0u32..9).prop_map(Token::new),
        label in (0u32..5).prop_map(Token::new),
    ) {
        let record = NodeRecord { labels: labels.clone(), properties };
        prop_assert_eq!(EncodedNode(&record.encode()).labels().unwrap(), labels.clone());
        prop_assert_eq!(EncodedNode(&record.encode()).has_label(label).unwrap(), labels.contains(&label));
        let read = EncodedNode(&record.encode()).property(key).unwrap();
        prop_assert!(agrees(read, record.properties.get(&key)));
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
