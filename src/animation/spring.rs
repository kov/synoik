// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct SpringParams {
    pub damping: f64,
    pub mass: f64,
    pub stiffness: f64,
    pub epsilon: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Spring {
    pub from: f64,
    pub to: f64,
    pub initial_velocity: f64,
    pub params: SpringParams,
}

impl SpringParams {
    pub fn new(damping_ratio: f64, stiffness: f64, epsilon: f64) -> Self {
        let damping_ratio = damping_ratio.max(0.);
        let stiffness = stiffness.max(0.);
        let epsilon = epsilon.max(0.);

        let mass = 1.;
        let critical_damping = 2. * (mass * stiffness).sqrt();
        let damping = damping_ratio * critical_damping;

        Self {
            damping,
            mass,
            stiffness,
            epsilon,
        }
    }
}

impl Spring {
    pub fn value_at(&self, t: Duration) -> f64 {
        self.oscillate(t.as_secs_f64())
    }

    // Based on libadwaita (LGPL-2.1-or-later):
    // https://gitlab.gnome.org/GNOME/libadwaita/-/blob/1.4.4/src/adw-spring-animation.c,
    // which itself is based on (MIT):
    // https://github.com/robb/RBBAnimation/blob/master/RBBAnimation/RBBSpringAnimation.m
    /// Computes and returns the duration until the spring is at rest.
    pub fn duration(&self) -> Duration {
        const DELTA: f64 = 0.001;

        let beta = self.params.damping / (2. * self.params.mass);

        if beta.abs() <= f64::EPSILON || beta < 0. {
            return Duration::MAX;
        }

        if (self.to - self.from).abs() <= f64::EPSILON {
            return Duration::ZERO;
        }

        let omega0 = (self.params.stiffness / self.params.mass).sqrt();

        // As first ansatz for the overdamped solution,
        // and general estimation for the oscillating ones
        // we take the value of the envelope when it's < epsilon.
        let mut x0 = -self.params.epsilon.ln() / beta;

        // f64::EPSILON is too small for this specific comparison, so we use
        // f32::EPSILON even though it's doubles.
        let critically_damped = (beta - omega0).abs() <= f64::from(f32::EPSILON);
        if critically_damped || beta < omega0 {
            if !critically_damped || !x0.is_finite() {
                return Duration::from_secs_f64(x0);
            }

            // A critically damped spring decays as `(A + B t) * e^(-beta t)`, and that linear
            // factor outlives the bare envelope the estimate above is taken from: at `x0` the
            // spring is still around `ln(1/epsilon) * epsilon` short of its target.
            //
            // That is not a slightly-early finish. `Animation::value_at` returns `to` outright
            // once the duration is up, so whatever the spring had left to travel is taken in one
            // step, on the frame the animation is retired. Measured on a workspace switch at
            // `org.synoik.animations speed 0.3`: the render index moved ~0.00016 per frame all
            // the way down the tail and then 0.00106 in the final frame — six frames of travel
            // at once, 3 physical pixels of the row jumping into place.
            //
            // Solve for the time the *actual* curve is within epsilon instead. Each pass feeds
            // the current estimate back through `|x0| + |slope| t = epsilon * e^(beta t)`, which
            // approaches from below and settles in a handful of iterations.
            let displacement = self.from - self.to;
            let slope = (beta * displacement + self.initial_velocity).abs();
            let displacement = displacement.abs();

            let mut t = x0;
            for _ in 0..100 {
                if (self.to - self.oscillate(t)).abs() <= self.params.epsilon {
                    break;
                }

                let next = ((displacement + slope * t) / self.params.epsilon).ln() / beta;
                if !next.is_finite() || next <= t {
                    break;
                }
                t = next;
            }

            return Duration::from_secs_f64(t);
        }

        // Since the overdamped solution decays way slower than the envelope
        // we need to use the value of the oscillation itself.
        // Newton's root finding method is a good candidate in this particular case:
        // https://en.wikipedia.org/wiki/Newton%27s_method
        let mut y0 = self.oscillate(x0);
        let m = (self.oscillate(x0 + DELTA) - y0) / DELTA;

        let mut x1 = (self.to - y0 + m * x0) / m;
        let mut y1 = self.oscillate(x1);

        let mut i = 0;
        while (self.to - y1).abs() > self.params.epsilon {
            if i > 1000 {
                return Duration::ZERO;
            }

            x0 = x1;
            y0 = y1;

            let m = (self.oscillate(x0 + DELTA) - y0) / DELTA;

            x1 = (self.to - y0 + m * x0) / m;
            y1 = self.oscillate(x1);

            // Overdamped springs have some numerical stability issues...
            if !y1.is_finite() {
                return Duration::from_secs_f64(x0);
            }

            i += 1;
        }

        Duration::from_secs_f64(x1)
    }

    /// Computes and returns the duration until the spring reaches its target position.
    pub fn clamped_duration(&self) -> Option<Duration> {
        let beta = self.params.damping / (2. * self.params.mass);

        if beta.abs() <= f64::EPSILON || beta < 0. {
            return Some(Duration::MAX);
        }

        if (self.to - self.from).abs() <= f64::EPSILON {
            return Some(Duration::ZERO);
        }

        // The first frame is not that important and we avoid finding the trivial 0 for in-place
        // animations.
        let mut i = 1u16;
        let mut y = self.oscillate(f64::from(i) / 1000.);

        while (self.to - self.from > f64::EPSILON && self.to - y > self.params.epsilon)
            || (self.from - self.to > f64::EPSILON && y - self.to > self.params.epsilon)
        {
            if i > 3000 {
                return None;
            }

            i += 1;
            y = self.oscillate(f64::from(i) / 1000.);
        }

        Some(Duration::from_millis(u64::from(i)))
    }

    /// Returns the spring position at a given time in seconds.
    fn oscillate(&self, t: f64) -> f64 {
        let b = self.params.damping;
        let m = self.params.mass;
        let k = self.params.stiffness;
        let v0 = self.initial_velocity;

        let beta = b / (2. * m);
        let omega0 = (k / m).sqrt();

        let x0 = self.from - self.to;

        let envelope = (-beta * t).exp();

        // Solutions of the form C1*e^(lambda1*x) + C2*e^(lambda2*x)
        // for the differential equation m*ẍ+b*ẋ+kx = 0

        // f64::EPSILON is too small for this specific comparison, so we use
        // f32::EPSILON even though it's doubles.
        if (beta - omega0).abs() <= f64::from(f32::EPSILON) {
            // Critically damped.
            self.to + envelope * (x0 + (beta * x0 + v0) * t)
        } else if beta < omega0 {
            // Underdamped.
            let omega1 = ((omega0 * omega0) - (beta * beta)).sqrt();

            self.to
                + envelope
                    * (x0 * (omega1 * t).cos() + ((beta * x0 + v0) / omega1) * (omega1 * t).sin())
        } else {
            // Overdamped.
            let omega2 = ((beta * beta) - (omega0 * omega0)).sqrt();

            self.to
                + envelope
                    * (x0 * (omega2 * t).cosh() + ((beta * x0 + v0) / omega2) * (omega2 * t).sinh())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A critically damped spring's duration must be long enough that the spring has actually
    /// arrived, because nothing eases it in afterwards: `Animation::value_at` returns `to` the
    /// moment the duration is up, so a duration that ends early does not shorten the animation,
    /// it *truncates* it — the remaining distance is taken in a single frame.
    ///
    /// The estimate this replaced came from the decay envelope alone, which ignores the linear
    /// term critical damping carries, and landed `ln(1/epsilon) * epsilon` short: ~9.2e-4 against
    /// a 1e-4 epsilon, nine times the tolerance it claimed to honour.
    #[test]
    fn a_critically_damped_spring_has_arrived_when_its_duration_is_up() {
        // The workspace switch's own parameters.
        let spring = Spring {
            from: 0.,
            to: 1.,
            initial_velocity: 0.,
            params: SpringParams::new(1., 1000., 0.0001),
        };

        let residual = (spring.to - spring.value_at(spring.duration())).abs();
        assert!(
            residual <= spring.params.epsilon,
            "the spring is still {residual} from its target when its duration ends, \
             which is more than the {} epsilon it is meant to settle within",
            spring.params.epsilon,
        );
    }

    #[test]
    fn overdamped_spring_equal_from_to_nan() {
        let spring = Spring {
            from: 0.,
            to: 0.,
            initial_velocity: 0.,
            params: SpringParams::new(1.15, 850., 0.0001),
        };
        let _ = spring.duration();
        let _ = spring.clamped_duration();
        let _ = spring.value_at(Duration::ZERO);
    }

    #[test]
    fn overdamped_spring_duration_panic() {
        let spring = Spring {
            from: 0.,
            to: 1.,
            initial_velocity: 0.,
            params: SpringParams::new(6., 1200., 0.0001),
        };
        let _ = spring.duration();
        let _ = spring.clamped_duration();
        let _ = spring.value_at(Duration::ZERO);
    }
}
