//! A short run of the mutation fuzzer (`examples/fuzz.rs`; run it longer by hand): mutated
//! fixture scripts must never panic the engine nor run for long.

#[path = "../examples/fuzz.rs"]
#[allow(dead_code)]
mod fuzz;

#[test]
fn mutated_scripts_never_panic() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let bad = fuzz::run(3000, 42, |_, _, _| {});
    std::panic::set_hook(prev);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
