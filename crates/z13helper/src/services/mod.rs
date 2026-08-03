//! Background services. These modules never touch GTK widgets from workers.

#[path = "../controller.rs"]
pub mod controller;
#[path = "../power.rs"]
pub mod power;
#[path = "../subscribe.rs"]
pub mod subscribe;
#[path = "../worker.rs"]
pub mod worker;
