//! `hsfs` (29-hsfs.md §32.4, 04-decisions.md D361): hyperstorctl, run by
//! that name, whose commands are `dataset`'s in the order `zfs` users
//! type them (`hyperstor_ctl::hsfs_args`).

#[path = "main.rs"]
mod hyperstorctl;

fn main() {
    hyperstorctl::main()
}
