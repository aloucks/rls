// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
#![allow(dead_code, unused_imports)]

pub mod sample;

use sample::Foo;
use sample::Bar;
use sample::Baz;
use sample::NewType;

#[derive(Default)]
pub struct Thing {
    /// A field
    /// 
    /// Maecenas urna ligula, suscipit sed.
    thing: String
}

/// Some function
/// 
/// Drops `foo` in place.
pub fn drop_foo<T>(foo: Foo<T>) {
    drop(foo);

    let baz = Bar::Baz;
    drop(baz);
}

/// Drops the boxed result
pub fn drop_boxed<T>(boxed: Box<Vec<(String, Foo<T>)>>) {
    drop(boxed);
}

fn main() {
    let foo = sample::foo();
    let foo_copy = foo.make_copy();
    let newtype = NewType::new();
    let string = String::from("string");
    let result = newtype.bar(string.clone(), foo_copy);
    let boxed = Box::new(result);
    println!("{:?}", boxed);

    std::thread::spawn(move || {
        drop_foo(foo);
        drop_boxed(boxed);
        let hello = "world";
        let mut thing = Thing::default();
        thing.thing = hello.into();
    });
}

pub mod test_tooltip;
pub mod test_tooltip_mod;
pub mod test_tooltip_mod_use;
pub mod test_tooltip_std;