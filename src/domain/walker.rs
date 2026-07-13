use optionstratlib::Positive;
use optionstratlib::chains::OptionChain;
use optionstratlib::simulation::{WalkParams, WalkType, WalkTypeAble};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, StandardNormal};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::{Decimal, MathematicalOps};
use std::error::Error;
use std::sync::Mutex;

/// Walker struct for implementing WalkTypeAble.
///
/// It owns its random number generator so that, when created with a seed,
/// every stochastic method draws from a deterministic sequence: the same
/// seed and parameters always produce the same walk.
pub(crate) struct Walker {
    rng: Mutex<StdRng>,
}

impl Walker {
    pub(crate) fn new() -> Self {
        Walker {
            rng: Mutex::new(StdRng::from_os_rng()),
        }
    }

    pub(crate) fn new_with_seed(seed: u64) -> Self {
        Walker {
            rng: Mutex::new(StdRng::seed_from_u64(seed)),
        }
    }

    /// Draws a standard normal sample from the walker's own RNG instead of the
    /// process-wide thread-local one used by optionstratlib's default methods.
    fn normal_sample(&self) -> Decimal {
        let mut rng = self.rng.lock().unwrap();
        let z: f64 = StandardNormal.sample(&mut *rng);
        Decimal::from_f64(z).unwrap_or(Decimal::ZERO)
    }

    /// Ornstein-Uhlenbeck path drawn from the walker's RNG. Mirrors
    /// optionstratlib's `generate_ou_process`, which cannot be seeded.
    fn ou_process(
        &self,
        x0: Positive,
        mu: Positive,
        theta: Positive,
        volatility: Positive,
        dt: Positive,
        steps: usize,
    ) -> Vec<Positive> {
        let sqrt_dt = dt.sqrt();
        let mut x = x0.to_dec();
        let mut result = Vec::with_capacity(steps);
        result.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));

        for _ in 1..steps {
            let dw = self.normal_sample() * sqrt_dt;
            let drift = theta * mu.sub_or_zero(&x) * dt;
            let diffusion = volatility * dw;
            x += drift + diffusion;
            x = x.max(Decimal::ZERO);
            result.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));
        }

        result
    }
}

/// Re-implementation of every stochastic method of `WalkTypeAble` so samples
/// come from the walker's own (optionally seeded) RNG. The math replicates
/// optionstratlib's default implementations; only the source of randomness
/// changes. `historical` keeps the default implementation as it draws no
/// random numbers.
impl WalkTypeAble<Positive, OptionChain> for Walker {
    fn brownian(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::Brownian {
                dt,
                drift,
                volatility,
            } => {
                let mut values = Vec::with_capacity(params.size + 1);
                let mut x: Positive = params.ystep_as_positive();
                values.push(x);
                let sigma_abs = volatility * x;
                let sqrt_dt = dt.to_f64().sqrt();

                for _ in 1..params.size {
                    let z = self.normal_sample();
                    let diffusion = sigma_abs * sqrt_dt * z;
                    let drift_term = drift * dt;
                    x += drift_term + diffusion;
                    values.push(x);
                }

                Ok(values)
            }
            _ => Err("Invalid walk type for Brownian motion".into()),
        }
    }

    fn geometric_brownian(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::GeometricBrownian {
                dt,
                drift,
                volatility,
            } => {
                let mut values = Vec::with_capacity(params.size);
                let mut current_value: Positive = params.ystep_as_positive();
                values.push(current_value);
                let sqrt_dt = dt.sqrt();

                for _ in 1..params.size {
                    let diffusion = self.normal_sample() * volatility * sqrt_dt;
                    let drift_term = (drift * dt) + diffusion;
                    current_value *= Decimal::exp(&drift_term);
                    values.push(current_value);
                }
                Ok(values)
            }
            _ => Err("Invalid walk type for Geometric Brownian motion".into()),
        }
    }

    fn log_returns(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::LogReturns {
                dt,
                expected_return,
                volatility,
                autocorrelation,
            } => {
                let mut values = Vec::with_capacity(params.size + 1);
                let mut price: Positive = params.ystep_as_positive();
                values.push(price);

                let sqrt_dt = dt.to_f64().sqrt();
                let mut prev_log_ret = Decimal::ZERO;

                for _ in 1..params.size {
                    let z = self.normal_sample();
                    let diffusion = volatility * sqrt_dt * z;
                    let mut log_ret = (expected_return * dt) + diffusion;

                    if let Some(ac) = autocorrelation {
                        assert!((-Decimal::ONE..=Decimal::ONE).contains(&ac));
                        log_ret += ac * prev_log_ret;
                    }

                    price *= log_ret.exp();
                    values.push(price);

                    prev_log_ret = log_ret;
                }
                Ok(values)
            }
            _ => Err("Invalid walk type for Log Returns motion".into()),
        }
    }

    fn mean_reverting(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::MeanReverting {
                dt,
                volatility,
                speed,
                mean,
            } => {
                let sigma_abs = volatility * mean;
                Ok(self.ou_process(
                    params.ystep_as_positive(),
                    mean,
                    speed,
                    sigma_abs,
                    dt,
                    params.size,
                ))
            }
            _ => Err("Invalid walk type for Mean Reverting motion".into()),
        }
    }

    fn jump_diffusion(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::JumpDiffusion {
                dt,
                drift,
                volatility,
                intensity,
                jump_mean,
                jump_volatility,
            } => {
                let mut values = Vec::with_capacity(params.size + 1);
                let mut x: Decimal = params.ystep_as_positive().to_dec();
                values.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));

                let sqrt_dt = dt.sqrt();
                let lambda_dt = intensity * dt;

                for _ in 1..params.size {
                    let z = self.normal_sample();
                    let sigma_abs = volatility * x;
                    let diffusion = sigma_abs * sqrt_dt * z;

                    let drift_term = drift * dt;
                    let jump = if self.normal_sample() < lambda_dt.to_dec() {
                        // Bernoulli(λdt)
                        jump_mean + jump_volatility * self.normal_sample()
                    } else {
                        Decimal::ZERO
                    };

                    x += drift_term + diffusion + jump;
                    x = x.max(Decimal::ZERO);
                    values.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));
                }

                Ok(values)
            }
            _ => Err("Invalid walk type for Jump Diffusion motion".into()),
        }
    }

    fn garch(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::Garch {
                dt,
                drift,
                volatility,
                alpha,
                beta,
            } => {
                if alpha + beta >= Decimal::ONE {
                    return Err("alpha + beta must be < 1 for stationarity".into());
                }

                let mut path = Vec::with_capacity(params.size + 1);
                let mut price = params.ystep_as_positive().to_dec();
                path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));

                let mut var = volatility * volatility; // σ₀²
                let mut prev_eps2 = Decimal::ZERO;
                let omega = volatility.powu(2) * (Decimal::ONE - alpha - beta);

                let sqrt_dt = dt.to_f64().sqrt();

                for _ in 1..params.size {
                    var = omega + alpha * prev_eps2 + beta * var;

                    let z = self.normal_sample();
                    let eps = var.sqrt() * sqrt_dt * z; // εₜ

                    let ret = drift * dt + eps;

                    price *= (ret).exp();
                    path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));

                    prev_eps2 = eps.powu(2).to_dec(); // εₜ²
                }
                Ok(path)
            }
            _ => Err("Invalid walk type for GARCH model".into()),
        }
    }

    fn heston(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::Heston {
                dt,
                drift,
                volatility,
                kappa,
                theta,
                xi,
                rho,
            } => {
                if rho < -Decimal::ONE || rho > Decimal::ONE {
                    return Err("Correlation rho must be between -1 and 1".into());
                }

                let mut values = Vec::with_capacity(params.size);
                let mut price: Positive = params.ystep_as_positive();

                let mut variance = volatility.to_dec() * volatility.to_dec();

                values.push(price);

                for _ in 0..params.size - 1 {
                    // Generate correlated random numbers
                    let z1 = self.normal_sample();
                    let z2 = rho * z1
                        + (Decimal::ONE - rho * rho).sqrt().unwrap() * self.normal_sample();

                    // Ensure variance stays positive (modified Euler scheme with truncation)
                    let variance_new = (variance
                        + kappa.to_dec() * (theta.to_dec() - variance) * dt.to_dec()
                        + xi.to_dec()
                            * variance.sqrt().unwrap()
                            * z2
                            * dt.to_dec().sqrt().unwrap())
                    .max(Decimal::ZERO);

                    // Update price using the average variance over the step
                    let avg_variance = (variance + variance_new) / Decimal::TWO;
                    let price_change = drift * dt.to_dec()
                        + avg_variance.sqrt().unwrap() * z1 * dt.to_dec().sqrt().unwrap();

                    price *= (price_change).exp();
                    variance = variance_new;

                    values.push(price);
                }

                Ok(values)
            }
            _ => Err("Invalid walk type for Heston model".into()),
        }
    }

    fn custom(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, Box<dyn Error>> {
        match params.walk_type {
            WalkType::Custom {
                dt,
                drift,
                volatility,
                vov,
                vol_speed,
                vol_mean,
            } => {
                let vols = self.ou_process(volatility, vol_mean, vol_speed, vov, dt, params.size);

                let sqrt_dt = dt.sqrt();
                let mut price = params.ystep_as_positive().to_dec();
                let mut path = Vec::with_capacity(params.size + 1);
                path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));

                for &vol in vols.iter().take(params.size - 1) {
                    let z = self.normal_sample();
                    let sigma_abs = vol * price;
                    let random_step = z * sigma_abs * sqrt_dt;

                    price += drift * dt + random_step;
                    price = price.max(Decimal::ZERO);
                    path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));
                }

                Ok(path)
            }
            _ => Err("Invalid walk type for Custom motion".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use optionstratlib::pos;

    fn sample_series(walker: &Walker, n: usize) -> Vec<Decimal> {
        (0..n).map(|_| walker.normal_sample()).collect()
    }

    #[test]
    fn test_seeded_walkers_produce_identical_samples() {
        let a = Walker::new_with_seed(42);
        let b = Walker::new_with_seed(42);
        assert_eq!(sample_series(&a, 100), sample_series(&b, 100));
    }

    #[test]
    fn test_different_seeds_produce_different_samples() {
        let a = Walker::new_with_seed(42);
        let b = Walker::new_with_seed(43);
        assert_ne!(sample_series(&a, 100), sample_series(&b, 100));
    }

    #[test]
    fn test_seeded_ou_process_is_reproducible() {
        let a = Walker::new_with_seed(7);
        let b = Walker::new_with_seed(7);
        let pa = a.ou_process(
            pos!(100.0),
            pos!(100.0),
            pos!(0.5),
            pos!(0.2),
            pos!(0.01),
            50,
        );
        let pb = b.ou_process(
            pos!(100.0),
            pos!(100.0),
            pos!(0.5),
            pos!(0.2),
            pos!(0.01),
            50,
        );
        assert_eq!(pa, pb);
    }
}
