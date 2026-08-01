//! Placeholder — implemented in the curve milestone.
pub const HIGH_TDP_MIN_PWM: i32 = 204;
pub const TDP_MAX_SAFE: u32 = 75;

pub fn floor_pwm(pl1: u32) -> i32 {
    if pl1 > TDP_MAX_SAFE {
        HIGH_TDP_MIN_PWM
    } else {
        0
    }
}
