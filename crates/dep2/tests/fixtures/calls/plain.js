function target() {} // @fn js_target
function run(callback) { // @fn js_run
  callback(); // @call js_target
}
function main() { // @fn js_main
  run(target); // @call js_run
  const fn = function recursive() { // @fn js_recursive
    recursive(); // @call js_recursive @caller js_recursive
  };
  fn(); // @call js_recursive
  const method = { run() { // @fn js_method
    target(); // @call js_target
  }};
  method.run(); // @call js_method
  let assigned;
  assigned = target;
  assigned(); // @call js_target
  {
    var hoisted = target;
  }
  hoisted(); // @call js_target
  {
    let target;
    target(); // @call ?
  }
}
function more() { // @fn js_more
  const box = {};
  box.run = target;
  box['run'](); // @call js_target
  const [first] = [target];
  first(); // @call js_target
  const dense = [target];
  dense[0](); // @call js_target
  const values = [, target];
  const [, second] = values;
  second(); // @call js_target
  values[1](); // @call js_target
  const copied = { ...box };
  copied.run(); // @call js_target
  target(run); // @call js_target
  try {
    throw target;
  } catch (target) {
    target(); // @call ?
  }
}
function loopShadow() { // @fn loop_shadow
  for (const target of unknown) {
    target(); // @call ?
  }
}

function loopValues() { // @fn loop_values
  for (const callback of [target]) {
    callback(); // @call js_target
  }
}
