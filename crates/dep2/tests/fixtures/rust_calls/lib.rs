mod nested;
use nested::{chosen as imported, Item as ImportedItem};
fn left() {} // @fn left
fn right() {} // @fn right
fn same() {} // @fn outer_same
struct A { callback: fn() }
struct B;
impl A {
    fn new(callback: fn()) -> Self { // @fn a_new
        Self { callback }
    }
    fn run(&self) { // @fn a_run
        (self.callback)(); // @call left
    }
}
impl B {
    fn run(&self) {} // @fn b_run
}
fn invoke(first: fn(), second: fn()) { // @fn invoke
    first(); // @call left
    second(); // @call right
}
fn identity(callback: fn()) -> fn() { // @fn identity
    callback
}
fn scoped() { // @fn scoped
    same(); // @call outer_same
    let same = right;
    same(); // @call right
    let same = same;
    same(); // @call right
    {
        let same = left;
        same(); // @call left
    }
    same(); // @call right
}
fn main_calls() { // @fn main_calls
    invoke(left, right); // @call invoke
    let cb = identity(left); // @call identity
    cb(); // @call left
    let closure = || left(); // @fn closure @call left @caller closure
    closure(); // @call closure
    let a = A::new(left); // @call a_new
    let b = B;
    a.run(); // @call a_run
    b.run(); // @call b_run
    imported(); // @call nested_chosen
    nested::chosen(); // @call nested_chosen
    let item = ImportedItem;
    item.run(); // @call nested_run
}
fn typed(a: &A, b: &B) { // @fn typed
    a.run(); // @call a_run
    b.run(); // @call b_run
}
fn aliases() { // @fn aliases
    let mut callback: fn() = left;
    let ptr = &mut callback;
    *ptr = right;
    callback(); // @call left,right
    let pair = (left, right);
    let (first, second) = pair;
    first(); // @call left
    second(); // @call right
    (pair.0)(); // @call left
    let mut a = A { callback: left };
    let ptr = &mut a.callback;
    *ptr = right;
    (a.callback)(); // @call left,right
}
mod other {
    pub fn same() {} // @fn other_same
    pub struct A;
    impl A {
        pub fn run(&self) {} // @fn other_run
    }
    pub fn caller() { // @fn other_caller
        same(); // @call other_same
        super::left(); // @call left
        crate::right(); // @call right
        let a = A;
        a.run(); // @call other_run
    }
}
fn closure_scope() { // @fn closure_scope
    let captured = left;
    let cb = || captured(); // @fn captured_closure @call left
    let captured = right;
    cb(); // @call captured_closure
    captured(); // @call right
}
trait Runnable { fn run(&self); }
impl Runnable for A {
    fn run(&self) {} // @fn trait_run
}
fn trait_receiver(a: &dyn Runnable) { // @fn trait_receiver
    a.run(); // @call ?
}
fn pass_trait() { // @fn pass_trait
    let a = A { callback: left };
    trait_receiver(&a); // @call trait_receiver
}
use renamed::{chosen as from_crate, Thing as AnotherThing};
fn workspace_calls() { // @fn workspace_calls
    from_crate(); // @call dependency_chosen
    renamed::chosen(); // @call dependency_chosen
    let thing = AnotherThing;
    thing.run(); // @call dependency_run
    nested::child_calls(); // @call child_calls
}
fn generic_identity<T>(value: T) -> T { // @fn generic_identity
    value
}
fn generic_calls() { // @fn generic_calls
    let cb = generic_identity::<fn()>(left); // @call generic_identity
    cb(); // @call left
    let invoke = |cb: fn()| cb(); // @fn closure_invoke @call right
    invoke(right); // @call closure_invoke
}
fn loop_values() { // @fn loop_values
    for same in [left as fn(), right as fn()] {
        same(); // @call left,right
    }
}
fn patterns(value: A, option: Option<fn()>) { // @fn patterns
    let A { callback } = value;
    callback(); // @call left
    if let Some(same) = option {
        same(); // @call ?
    } else {
        same(); // @call outer_same
    }
    match option {
        Some(same) => same(), // @call ?
        None => same(), // @call outer_same
    }
}

fn pattern_caller() { // @fn pattern_caller
    patterns(A { callback: left }, None); // @call patterns
}
struct TraitHolder<'a> { value: &'a dyn Runnable }
fn field_trait(a: &A) { // @fn field_trait
    let holder = TraitHolder { value: a };
    holder.value.run(); // @call ?
}
