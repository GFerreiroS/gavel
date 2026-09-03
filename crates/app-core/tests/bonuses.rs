//! Real DB2 bonus-id examples pinned to the generated Shatari data revision.

use app_core::market::{Catalog, Copper, ItemId, TertiaryStat, decode, metadata};

const PEACEBLOOM: ItemId = ItemId(236_761);

#[test]
fn real_bonus_ids_decode_level_suffix_and_tertiary() {
    // 12825 is an era-curve level rule, 459 names suffix 13150, and 41 is
    // Leech in the source ItemModType table.
    let decoded = decode(PEACEBLOOM, &[12_825, 459, 41], None).expect("curated item");

    assert_eq!(decoded.item_level, 310);
    assert_eq!(decoded.name_suffix.as_deref(), Some("of the Fireflash"));
    assert_eq!(decoded.tertiary_stats, vec![TertiaryStat::Leech]);
}

#[test]
fn real_tertiary_bonus_ids_keep_their_stat_identity() {
    let decoded = decode(PEACEBLOOM, &[40, 41, 42, 43], None).expect("curated item");

    assert_eq!(
        decoded.tertiary_stats,
        vec![
            TertiaryStat::Speed,
            TertiaryStat::Leech,
            TertiaryStat::Avoidance,
            TertiaryStat::Indestructible,
        ]
    );
}

#[test]
fn catalog_boundary_keeps_variant_as_a_string_identity() {
    let decoded = Catalog::decode_variant(PEACEBLOOM, "12825,459,41").expect("valid variant");

    assert_eq!(decoded.item_level, 310);
    assert!(Catalog::decode_variant(PEACEBLOOM, "12825,nope").is_none());
}

#[test]
fn curated_offline_metadata_does_not_replace_runtime_tooltips() {
    let item = metadata(PEACEBLOOM).expect("curated item");

    assert_eq!(item.item, PEACEBLOOM);
    assert_eq!(item.icon.as_deref(), Some("inv_misc_herb_peacebloom"));
    assert_eq!(item.vendor_buy, None);
    assert_eq!(item.vendor_sell, Some(Copper(750)));
    assert_eq!(item.stack_size, 1_000);
    assert!(!item.bind_on_pickup);
    assert_eq!(item.expansion, Some(12));
    assert_eq!(Catalog::item_metadata(PEACEBLOOM), Some(item));
    assert!(metadata(ItemId(271_438)).is_none(), "known source-data gap");
}
