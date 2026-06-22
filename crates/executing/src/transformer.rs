use planning::collections::CollectionSignature;
use planning::flow::TransformationFlow;
use reading::arrangements::ArrangedDict;
use reading::arrangements::ArrangedSet;
use reading::rel::DoubleRel;
use reading::rel::Rel;
use reading::rel::Rel::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::jn::*;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::Data;
use macros::*;
use timely::order::TotalOrder;
use timely::progress::timestamp::Timestamp;

pub fn cartesian<'scope, T>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    row_map: &HashMap<Arc<CollectionSignature>, Arc<Rel<'scope, T>>>,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
) -> Arc<Rel<'scope, T>>
where
    T: Timestamp + Data + Lattice + TotalOrder,
{
    // differential 0.20: arrange_by_key is an inherent method on Collection.
    let rel_0 = row_map.get(large).expect("0 for cartesian");
    let rel_1 = row_map.get(small).expect("1 for cartesian");
    Arc::new(codegen_cartesian!())
}

pub fn kv_jn_kv<'scope, T>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    kv_map: &HashMap<
        Arc<CollectionSignature>,
        (Arc<DoubleRel<'scope, T>>, Arc<ArrangedDict<'scope, T>>),
    >,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
) -> Arc<Rel<'scope, T>>
where
    T: Timestamp + Data + Lattice + TotalOrder,
{
    let (_, dict_0) = kv_map.get(large).expect("0 for kv jn kv");
    let (_, dict_1) = kv_map.get(small).expect("1 for kv jn kv");
    Arc::new(codegen_jn!())
}

pub fn kv_jn_k<'scope, T>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    kv_map: &HashMap<
        Arc<CollectionSignature>,
        (Arc<DoubleRel<'scope, T>>, Arc<ArrangedDict<'scope, T>>),
    >,
    k_map: &HashMap<Arc<CollectionSignature>, (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
) -> Arc<Rel<'scope, T>>
where
    T: Timestamp + Data + Lattice + TotalOrder,
{
    assert!(iv1 == 0);
    let (_, dict_0) = kv_map.get(large).expect("dict for kv jn k");
    let (_, set_1) = k_map.get(small).expect("set for kv jn k");
    Arc::new(codegen_kv_k_jn!())
}

pub fn k_jn_k<'scope, T>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    k_map: &HashMap<Arc<CollectionSignature>, (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>)>,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
) -> Arc<Rel<'scope, T>>
where
    T: Timestamp + Data + Lattice + TotalOrder,
{
    assert!(iv0 == 0 && iv1 == 0);
    let (_, set_0) = k_map.get(large).expect("0 for k jn k");
    let (_, set_1) = k_map.get(small).expect("1 for k jn k");
    Arc::new(codegen_k_k_jn!())
}

pub fn kv_aj_k<'scope, T>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    kv_map: &HashMap<
        Arc<CollectionSignature>,
        (Arc<DoubleRel<'scope, T>>, Arc<ArrangedDict<'scope, T>>),
    >,
    k_map: &mut HashMap<
        Arc<CollectionSignature>,
        (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>),
    >,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
) -> Arc<Rel<'scope, T>>
where
    T: Timestamp + Data + Lattice + TotalOrder,
{
    assert!(iv1 == 0);
    if let Some((rel_1, set_1)) = k_map.get_mut(small) {
        *rel_1 = Arc::new(set_1.threshold());
        *set_1 = Arc::new(rel_1.arrange_set());
    } else {
        panic!("threshold for kv aj k");
    }
    let (_, dict_0) = kv_map.get(large).expect("dict for kv aj k");
    let (_, set_1) = k_map.get(small).expect("set for kv aj k");

    /* i32 version
        let substract_rel = codegen_kv_k_jn!().negate();
        Arc::new(codegen_kv_flatten!().concat(&substract_rel))
    */

    /* boolean version */
    Arc::new(codegen_kv_flatten!().subtract(&codegen_kv_k_jn!()))
}

pub fn k_aj_k<'scope, T>(
    large: &Arc<CollectionSignature>,
    small: &Arc<CollectionSignature>,
    k_map: &mut HashMap<
        Arc<CollectionSignature>,
        (Arc<Rel<'scope, T>>, Arc<ArrangedSet<'scope, T>>),
    >,
    ik0: usize,
    iv0: usize,
    iv1: usize,
    target: usize,
    flow: &TransformationFlow,
) -> Arc<Rel<'scope, T>>
where
    T: Timestamp + Data + Lattice + TotalOrder,
{
    assert!(iv0 == 0 && iv1 == 0);
    if let Some((rel_1, set_1)) = k_map.get_mut(small) {
        *rel_1 = Arc::new(set_1.threshold());
        *set_1 = Arc::new(rel_1.arrange_set());
    } else {
        panic!("threshold for k aj k");
    }
    let (_, set_0) = k_map.get(large).expect("0 for k aj k");
    let (_, set_1) = k_map.get(small).expect("1 for k aj k");

    /* i32 version
        let substract_rel = codegen_k_k_jn!().negate();
        Arc::new(codegen_k_flatten!().concat(&substract_rel))
    */

    /* boolean version */
    Arc::new(codegen_k_flatten!().subtract(&codegen_k_k_jn!()))
}
