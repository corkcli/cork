//! `collide-hook`: Cork's agent hooks as one native binary. The work is
//! in the library (lib.rs), which the free version's `cork` program
//! shares.

fn main() {
    cork_hooks::run(std::env::args().skip(1).collect())
}
