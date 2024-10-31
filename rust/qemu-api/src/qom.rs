// Copyright 2024, Linaro Limited
// Author(s): Manos Pitsidianakis <manos.pitsidianakis@linaro.org>
// SPDX-License-Identifier: GPL-2.0-or-later

//! Bindings to access QOM functionality from Rust.
//!
//! This module provides automatic creation and registration of `TypeInfo`
//! for classes that are written in Rust, and mapping between Rust traits
//! and QOM vtables.
//!
//! # Structure of a class
//!
//! A leaf class only needs a struct holding instance state. The struct must
//! implement the [`ObjectType`] trait, as well as any `*Impl` traits that exist
//! for its superclasses.
//!
//! If a class has subclasses, it will also provide a struct for instance data,
//! with the same characteristics as for concrete classes, but it also needs
//! additional components to support virtual methods:
//!
//! * a struct for class data, for example `DeviceClass`. This corresponds to
//!   the C "class struct" and holds the vtable that is used by instances of the
//!   class and its subclasses. It must start with its parent's class struct.
//!
//! * a trait for virtual method implementations, for example `DeviceImpl`.
//!   Child classes implement this trait to provide their own behavior for
//!   virtual methods. The trait's methods take `&self` to access instance data.
//!
//! * an implementation of [`ClassInitImpl`], for example
//!   `ClassInitImpl<DeviceClass>`. This fills the vtable in the class struct;
//!   the source for this is the `*Impl` trait; the associated consts and
//!   functions if needed are wrapped to map C types into Rust types.

use std::{ffi::CStr, os::raw::c_void};

pub use bindings::{Object, ObjectClass};

use crate::bindings::{self, TypeInfo};

unsafe extern "C" fn rust_instance_init<T: ObjectImpl>(obj: *mut Object) {
    // SAFETY: obj is an instance of T, since rust_instance_init<T>
    // is called from QOM core as the instance_init function
    // for class T
    unsafe { T::INSTANCE_INIT.unwrap()(&mut *obj.cast::<T>()) }
}

unsafe extern "C" fn rust_instance_post_init<T: ObjectImpl>(obj: *mut Object) {
    // SAFETY: obj is an instance of T, since rust_instance_post_init<T>
    // is called from QOM core as the instance_post_init function
    // for class T
    //
    // FIXME: it's not really guaranteed that there are no backpointers to
    // obj; it's quite possible that they have been created by instance_init().
    // The receiver should be &self, not &mut self.
    T::INSTANCE_POST_INIT.unwrap()(unsafe { &mut *obj.cast::<T>() })
}

unsafe extern "C" fn rust_class_init<T: ObjectType + ClassInitImpl<T::Class>>(
    klass: *mut ObjectClass,
    _data: *mut c_void,
) {
    // SAFETY: klass is a T::Class, since rust_class_init<T>
    // is called from QOM core as the class_init function
    // for class T
    T::class_init(unsafe { &mut *klass.cast::<T::Class>() })
}

/// Trait exposed by all structs corresponding to QOM objects.
///
/// # Safety
///
/// For classes declared in C:
///
/// - `Class` and `TYPE` must match the data in the `TypeInfo`;
///
/// - the first field of the struct must be of the instance type corresponding
///   to the superclass, as declared in the `TypeInfo`
///
/// - likewise, the first field of the `Class` struct must be of the class type
///   corresponding to the superclass
///
/// For classes declared in Rust and implementing [`ObjectImpl`]:
///
/// - the struct must be `#[repr(C)]`;
///
/// - the first field of the struct must be of the instance struct corresponding
///   to the superclass, which is `ObjectImpl::ParentType`
///
/// - likewise, the first field of the `Class` must be of the class struct
///   corresponding to the superclass, which is `ObjectImpl::ParentType::Class`.
pub unsafe trait ObjectType: Sized {
    /// The QOM class object corresponding to this struct.  This is used
    /// to automatically generate a `class_init` method.
    type Class;

    /// The name of the type, which can be passed to `object_new()` to
    /// generate an instance of this type.
    const TYPE_NAME: &'static CStr;
}

/// Trait a type must implement to be registered with QEMU.
pub trait ObjectImpl: ObjectType + ClassInitImpl<Self::Class> {
    /// The parent of the type.  This should match the first field of
    /// the struct that implements `ObjectImpl`:
    type ParentType: ObjectType;

    /// Whether the object can be instantiated
    const ABSTRACT: bool = false;
    const INSTANCE_FINALIZE: Option<unsafe extern "C" fn(obj: *mut Object)> = None;

    /// Function that is called to initialize an object.  The parent class will
    /// have already been initialized so the type is only responsible for
    /// initializing its own members.
    ///
    /// FIXME: The argument is not really a valid reference. `&mut
    /// MaybeUninit<Self>` would be a better description.
    const INSTANCE_INIT: Option<unsafe fn(&mut Self)> = None;

    /// Function that is called to finish initialization of an object, once
    /// `INSTANCE_INIT` functions have been called.
    const INSTANCE_POST_INIT: Option<fn(&mut Self)> = None;

    /// Called on descendent classes after all parent class initialization
    /// has occurred, but before the class itself is initialized.  This
    /// is only useful if a class is not a leaf, and can be used to undo
    /// the effects of copying the contents of the parent's class struct
    /// to the descendants.
    const CLASS_BASE_INIT: Option<
        unsafe extern "C" fn(klass: *mut ObjectClass, data: *mut c_void),
    > = None;

    const TYPE_INFO: TypeInfo = TypeInfo {
        name: Self::TYPE_NAME.as_ptr(),
        parent: Self::ParentType::TYPE_NAME.as_ptr(),
        instance_size: core::mem::size_of::<Self>(),
        instance_align: core::mem::align_of::<Self>(),
        instance_init: match Self::INSTANCE_INIT {
            None => None,
            Some(_) => Some(rust_instance_init::<Self>),
        },
        instance_post_init: match Self::INSTANCE_POST_INIT {
            None => None,
            Some(_) => Some(rust_instance_post_init::<Self>),
        },
        instance_finalize: Self::INSTANCE_FINALIZE,
        abstract_: Self::ABSTRACT,
        class_size: core::mem::size_of::<Self::Class>(),
        class_init: Some(rust_class_init::<Self>),
        class_base_init: Self::CLASS_BASE_INIT,
        class_data: core::ptr::null_mut(),
        interfaces: core::ptr::null_mut(),
    };

    // methods on ObjectClass
    const UNPARENT: Option<fn(&Self)> = None;
}

/// Internal trait used to automatically fill in a class struct.
///
/// Each QOM class that has virtual methods describes them in a
/// _class struct_.  Class structs include a parent field corresponding
/// to the vtable of the parent class, all the way up to [`ObjectClass`].
/// Each QOM type has one such class struct; this trait takes care of
/// initializing the `T` part of the class struct, for the type that
/// implements the trait.
///
/// Each struct will implement this trait with `T` equal to each
/// superclass.  For example, a device should implement at least
/// `ClassInitImpl<`[`DeviceClass`](crate::qdev::DeviceClass)`>` and
/// `ClassInitImpl<`[`ObjectClass`]`>`.  Such implementations are made
/// in one of two ways.
///
/// For most superclasses, `ClassInitImpl` is provided by the `qemu-api`
/// crate itself.  The Rust implementation of methods will come from a
/// trait like [`ObjectImpl`] or [`DeviceImpl`](crate::qdev::DeviceImpl),
/// and `ClassInitImpl` is provided by blanket implementations that
/// operate on all implementors of the `*Impl`* trait.  For example:
///
/// ```ignore
/// impl<T> ClassInitImpl<DeviceClass> for T
/// where
///     T: ClassInitImpl<ObjectClass> + DeviceImpl,
/// ```
///
/// The bound on `ClassInitImpl<ObjectClass>` is needed so that,
/// after initializing the `DeviceClass` part of the class struct,
/// the parent [`ObjectClass`] is initialized as well.
///
/// The other case is when manual implementation of the trait is needed.
/// This covers the following cases:
///
/// * if a class implements a QOM interface, the Rust code _has_ to define its
///   own class struct `FooClass` and implement `ClassInitImpl<FooClass>`.
///   `ClassInitImpl<FooClass>`'s `class_init` method will then forward to
///   multiple other `class_init`s, for the interfaces as well as the
///   superclass. (Note that there is no Rust example yet for using interfaces).
///
/// * for classes implemented outside the ``qemu-api`` crate, it's not possible
///   to add blanket implementations like the above one, due to orphan rules. In
///   that case, the easiest solution is to implement
///   `ClassInitImpl<YourSuperclass>` for each subclass and not have a
///   `YourSuperclassImpl` trait at all.
///
/// ```ignore
/// impl ClassInitImpl<YourSuperclass> for YourSubclass {
///     fn class_init(klass: &mut YourSuperclass) {
///         klass.some_method = Some(Self::some_method);
///         <Self as ClassInitImpl<SysBusDeviceClass>>::class_init(&mut klass.parent_class);
///     }
/// }
/// ```
///
///   While this method incurs a small amount of code duplication,
///   it is generally limited to the recursive call on the last line.
///   This is because classes defined in Rust do not need the same
///   glue code that is needed when the classes are defined in C code.
///   You may consider using a macro if you have many subclasses.
pub trait ClassInitImpl<T> {
    /// Initialize `klass` to point to the virtual method implementations
    /// for `Self`.  On entry, the virtual method pointers are set to
    /// the default values coming from the parent classes; the function
    /// can change them to override virtual methods of a parent class.
    ///
    /// The virtual method implementations usually come from another
    /// trait, for example [`DeviceImpl`](crate::qdev::DeviceImpl)
    /// when `T` is [`DeviceClass`](crate::qdev::DeviceClass).
    ///
    /// On entry, `klass`'s parent class is initialized, while the other fields
    /// are all zero; it is therefore assumed that all fields in `T` can be
    /// zeroed, otherwise it would not be possible to provide the class as a
    /// `&mut T`.  TODO: add a bound of [`Zeroable`](crate::zeroable::Zeroable)
    /// to T; this is more easily done once Zeroable does not require a manual
    /// implementation (Rust 1.75.0).
    fn class_init(klass: &mut T);
}

/// # Safety
///
/// We expect the FFI user of this function to pass a valid pointer that
/// can be downcasted to type `T`. We also expect the device is
/// readable/writeable from one thread at any time.
unsafe extern "C" fn rust_unparent_fn<T: ObjectImpl>(dev: *mut Object) {
    unsafe {
        assert!(!dev.is_null());
        let state = core::ptr::NonNull::new_unchecked(dev.cast::<T>());
        T::UNPARENT.unwrap()(state.as_ref());
    }
}

impl<T> ClassInitImpl<ObjectClass> for T
where
    T: ObjectImpl,
{
    fn class_init(oc: &mut ObjectClass) {
        if <T as ObjectImpl>::UNPARENT.is_some() {
            oc.unparent = Some(rust_unparent_fn::<T>);
        }
    }
}

unsafe impl ObjectType for Object {
    type Class = ObjectClass;
    const TYPE_NAME: &'static CStr =
        unsafe { CStr::from_bytes_with_nul_unchecked(bindings::TYPE_OBJECT) };
}
