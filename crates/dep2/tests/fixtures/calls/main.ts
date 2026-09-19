import primary, { chosen as imported, other } from './lib';
import * as ns from './lib';
import { forwarded } from './unused/..//barrel';
import { chosen as viaStar } from './star';

function left() {} // @fn left
function right() {} // @fn right
function same() {} // @fn outer_same
function invoke(cb: () => void) { // @fn invoke
  cb(); // @call left,right
}
function identity(cb: () => void) { // @fn identity
  return cb;
}
function scoped() { // @fn scoped
  function same() {} // @fn inner_same
  same(); // @call inner_same @caller scoped
  {
    const same = right;
    same(); // @call right
  }
  same(); // @call inner_same @caller scoped
}
function blocked(same: unknown) { // @fn blocked
  same(); // @call ?
}
function captured() { // @fn captured
  const selected = left;
  return () => selected(); // @fn closure @call left @caller closure
}
function main() { // @fn main
  same(); // @call outer_same
  scoped(); // @call scoped
  invoke(left); // @call invoke
  invoke(right); // @call invoke
  const chosen = identity(left); // @call identity
  chosen(); // @call left
  const a = { run: left };
  const b = { run: right };
  a.run(); // @call left
  b.run(); // @call right
  const alias = a;
  alias.run(); // @call left
  const { run: extracted } = a;
  extracted(); // @call left
  imported(); // @call lib_chosen
  other(); // @call lib_other
  ns.chosen(); // @call lib_chosen
  primary(); // @call lib_default
  forwarded(); // @call lib_chosen
  viaStar(); // @call lib_other
  ns.same(); // @call lib_same
  ns.privateFunction(); // @call ?
  const cast = right as () => void;
  cast(); // @call right
  const closure = captured(); // @call captured
  closure(); // @call closure
  unknown.run(); // @call ?
}
class A {
  constructor(cb: () => void) { // @fn ctor_a
    this.callback = cb;
  }
  run() { // @fn run_a
    this.callback(); // @call left
  }
}
class B {
  run() {} // @fn run_b
}
function classes() { // @fn classes
  const a = new A(left); // @call ctor_a
  const b = new B();
  a.run(); // @call run_a
  b.run(); // @call run_b
}

main(); // @call main @caller <module>

function positional(first: () => void, second: () => void) { // @fn positional
  first(); // @call left
  second(); // @call right
}
positional(left, /* comments are not arguments */ right); // @call positional
