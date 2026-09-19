pub fn chosen() {} // @fn nested_chosen
pub struct Item;
impl Item {
    pub fn run(&self) {} // @fn nested_run
}
pub fn alternate() {} // @fn nested_alternate
mod child;
pub fn child_calls() { // @fn child_calls
    child::target(); // @call child_target
}
