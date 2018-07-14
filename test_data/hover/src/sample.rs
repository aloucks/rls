// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Sample module
//! 
//! Lorem ipsum dolor sit amet, consectetur adipiscing elit. Maecenas
//! tincidunt tristique maximus. Sed venenatis urna vel sagittis tempus.
//! In hac habitasse platea dictumst.
//! 
//! # Examples
//! 
//! ```
//! # use sample;
//! 
//! let foo = sample::foo();
//! ```

use std::ops::Add;

/// Sample function
/// 
/// Ut molestie euismod mollis. In viverra tempor sollicitudin. In orci nisi,
/// ultricies in semper non, faucibus in nisl.
pub fn foo() -> Foo<u32> {
    Foo { t: 1 }
}

/// Sample struct
/// 
/// Proin finibus, risus ac egestas fermentum, enim diam bibendum magna,
/// vitae cursus dui odio a neque.
/// 
/// # Example
/// 
/// ```
/// let foo = Foo { t: 1 };
/// ```
#[derive(Debug)]
pub struct Foo<T> {
    /// The `t` field
    /// 
    /// Nulla tincidunt lobortis semper.
    pub t: T
}

/// Sample enum
/// 
/// Praesent at cursus ipsum, eu ornare dui. Nunc lobortis venenatis nulla eget
/// viverra. Nam tincidunt aliquam augue vitae consequat.
#[derive(Debug)]
pub enum Bar {
    /// The `Baz` variant
    /// 
    /// Aliquam erat volutpat.
    Baz
}


/// Sample newtype
/// 
/// Donec dictum eu arcu non commodo. Maecenas urna ligula, suscipit sed
/// ipsum tempus, auctor lobortis sapien.
#[derive(Debug)]
pub struct NewType(pub u32, f32);

impl NewType {
    /// Creates a `NewType`
    /// 
    /// # Examples
    /// 
    /// ```no_run
    /// let a = NewType::new();
    /// ```
    pub fn new() -> NewType {
        NewType(1, 2.0)
    }

    /// Bar method
    /// 
    /// Donec vestibulum ligula id vehicula luctus.
    pub fn bar<T: Copy + Add>(&self, the_really_long_name_string: String, the_really_long_name_foo: Foo<T>) -> Vec<(String, Foo<T>)> {
        let f = Foo { t: the_really_long_name_foo.t };
        let v = vec![(the_really_long_name_string, f)];
        v
    }
}

/// Sample trait
/// 
/// Pellentesque nisi neque, vehicula nec sapien lobortis, fermentum 
/// consequat lectus.
pub trait Baz<T> where T: Copy {
    /// Create a copy
    /// 
    /// Curabitur at est imperdiet, feugiat libero quis, viverra metus. 
    fn make_copy(&self) -> Self;
}

impl<T> Baz<T> for Foo<T> where T: Copy {
    fn make_copy(&self) -> Self {
        Foo { t: self.t }
    }
}


pub trait Qeh<T, U>
where T: Copy, 
U: Clone {
    
}

pub fn multiple_lines(
    s: String,
    i: i32
) {
    drop(s);
    drop(i);
}

pub fn bar() -> Bar {
    Bar::Baz
}