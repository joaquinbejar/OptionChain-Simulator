use optionstratlib::chains::OptionChain;
use optionstratlib::error::SimulationError;
use optionstratlib::simulation::{WalkParams, WalkPath, WalkType, WalkTypeAble};
use positive::Positive;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, StandardNormal};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::{Decimal, MathematicalOps};
use std::sync::{Arc, Mutex};

/// Walker struct for implementing WalkTypeAble.
///
/// It owns its random number generator so that, when created with a seed,
/// every stochastic method draws from a deterministic sequence: the same
/// seed and parameters always produce the same walk.
pub(crate) struct Walker {
    rng: Arc<Mutex<StdRng>>,
}

impl Walker {
    pub(crate) fn new() -> Self {
        Walker {
            rng: Arc::new(Mutex::new(StdRng::from_rng(&mut rand::rng()))),
        }
    }

    pub(crate) fn new_with_seed(seed: u64) -> Self {
        Walker {
            rng: Arc::new(Mutex::new(StdRng::seed_from_u64(seed))),
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
            let dw = self.normal_sample() * sqrt_dt.to_dec();
            let drift = (theta * mu.sub_or_zero(&x) * dt).to_dec();
            let diffusion = volatility.to_dec() * dw;
            x += drift + diffusion;
            x = x.max(Decimal::ZERO);
            result.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));
        }

        result
    }

    /// Seeded GARCH(1,1) kernel mirroring optionstratlib's `garch_walk`.
    fn garch_walk_seeded(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
        match params.walk_type {
            WalkType::Garch {
                dt,
                drift,
                volatility,
                alpha,
                beta,
            } => {
                if alpha + beta >= Decimal::ONE {
                    return Err(SimulationError::GarchStationarity { alpha, beta });
                }

                let mut path = Vec::with_capacity(params.size + 1);
                let mut vols = Vec::with_capacity(params.size + 1);
                let mut price = params.ystep_as_positive()?.to_dec();
                path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));
                vols.push(volatility);

                let mut var = volatility * volatility; // σ₀²
                let mut prev_eps2 = Decimal::ZERO;
                let omega = volatility.powu(2) * (Decimal::ONE - alpha - beta);

                let sqrt_dt = dt.to_f64().sqrt();
                let sqrt_dt_dec = Decimal::from_f64(sqrt_dt).ok_or_else(|| {
                    SimulationError::non_finite("simulation::garch::sqrt_dt", sqrt_dt)
                })?;

                for _ in 1..params.size {
                    var = omega + alpha * prev_eps2 + beta * var;

                    let z = self.normal_sample();
                    let eps = z * var.sqrt() * sqrt_dt_dec; // εₜ

                    let ret = drift * dt + eps;

                    price *= (ret).exp();
                    path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));
                    vols.push(var.sqrt());

                    prev_eps2 = eps.powu(2);
                }
                Ok(WalkPath {
                    prices: path,
                    vols: Some(vols),
                })
            }
            _ => Err(SimulationError::InvalidWalkType { expected: "GARCH" }),
        }
    }

    /// Seeded Heston kernel mirroring optionstratlib's `heston_walk`.
    fn heston_walk_seeded(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
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
                    return Err(SimulationError::InvalidCorrelation { rho });
                }

                let mut values = Vec::with_capacity(params.size);
                let mut vols = Vec::with_capacity(params.size);
                let mut price: Positive = params.ystep_as_positive()?;

                let mut variance = volatility.to_dec() * volatility.to_dec();

                values.push(price);
                vols.push(volatility);

                let dt_sqrt = dt.to_dec().sqrt().ok_or_else(|| {
                    SimulationError::walk_error("Heston: sqrt(dt) failed (overflow)")
                })?;
                let one_minus_rho_sq_sqrt = (Decimal::ONE - rho * rho).sqrt().ok_or_else(|| {
                    SimulationError::walk_error(
                        "Heston: sqrt(1 - rho^2) failed (rho out of range or overflow)",
                    )
                })?;
                for _ in 0..params.size - 1 {
                    let z1 = self.normal_sample();
                    let z2 = rho * z1 + one_minus_rho_sq_sqrt * self.normal_sample();

                    let variance_sqrt = variance.sqrt().ok_or_else(|| {
                        SimulationError::walk_error("Heston: sqrt(variance) failed (overflow)")
                    })?;
                    let variance_new = (variance
                        + kappa.to_dec() * (theta.to_dec() - variance) * dt.to_dec()
                        + xi.to_dec() * variance_sqrt * z2 * dt_sqrt)
                        .max(Decimal::ZERO);

                    let avg_variance = (variance + variance_new) / Decimal::TWO;
                    let avg_variance_sqrt = avg_variance.sqrt().ok_or_else(|| {
                        SimulationError::walk_error("Heston: sqrt(avg_variance) failed (overflow)")
                    })?;
                    let price_change = drift * dt.to_dec() + avg_variance_sqrt * z1 * dt_sqrt;

                    price *= (price_change).exp();
                    variance = variance_new;

                    values.push(price);
                    let vol_step = variance.sqrt().ok_or_else(|| {
                        SimulationError::walk_error("Heston: sqrt(variance) failed (overflow)")
                    })?;
                    vols.push(Positive::new_decimal(vol_step).unwrap_or(Positive::ZERO));
                }

                Ok(WalkPath {
                    prices: values,
                    vols: Some(vols),
                })
            }
            _ => Err(SimulationError::InvalidWalkType { expected: "Heston" }),
        }
    }

    /// Seeded Custom (mean-reverting volatility) kernel mirroring
    /// optionstratlib's `custom_walk`.
    fn custom_walk_seeded(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
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
                let mut price = params.ystep_as_positive()?.to_dec();
                let mut path = Vec::with_capacity(params.size + 1);
                let mut vols_out = Vec::with_capacity(params.size + 1);
                path.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));
                vols_out.push(volatility);

                for &vol in vols.iter().take(params.size - 1) {
                    let z = self.normal_sample();
                    let sigma_abs = vol.to_dec() * price;
                    let random_step = z * sigma_abs * sqrt_dt.to_dec();

                    price += drift * dt + random_step;
                    path.push(
                        Positive::new_decimal(price.max(Decimal::ZERO)).unwrap_or(Positive::ZERO),
                    );
                    vols_out.push(vol);
                }

                Ok(WalkPath {
                    prices: path,
                    vols: Some(vols_out),
                })
            }
            _ => Err(SimulationError::InvalidWalkType { expected: "Custom" }),
        }
    }

    /// Seeded Telegraph kernel mirroring optionstratlib's `telegraph_walk`.
    fn telegraph_walk_seeded(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
        match params.walk_type {
            WalkType::Telegraph {
                dt,
                drift,
                volatility,
                lambda_up,
                lambda_down,
                vol_multiplier_up,
                vol_multiplier_down,
            } => {
                let mut values = Vec::with_capacity(params.size);
                let mut vols = Vec::with_capacity(params.size);
                let mut price = params.ystep_as_positive()?.to_dec();
                values.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));
                vols.push(volatility);

                // Initialize telegraph state randomly
                let mut state: i8 = if self.normal_sample().to_f64().unwrap_or(0.0) < 0.0 {
                    1
                } else {
                    -1
                };

                let sqrt_dt = dt.sqrt();
                let vol_mult_up = vol_multiplier_up.unwrap_or(Positive::ONE);
                let vol_mult_down = vol_multiplier_down.unwrap_or(Positive::ONE);

                for _ in 1..params.size {
                    let lambda = if state == 1 {
                        lambda_down.to_dec()
                    } else {
                        lambda_up.to_dec()
                    };

                    let transition_prob = Decimal::ONE - (-lambda * dt.to_dec()).exp();

                    // Check for state transition using uniform random sample
                    let uniform_sample = (self.normal_sample().abs() + Decimal::ONE) / Decimal::TWO;
                    if uniform_sample < transition_prob {
                        state *= -1;
                    }

                    let current_vol = if state == 1 {
                        volatility * vol_mult_up
                    } else {
                        volatility * vol_mult_down
                    };

                    let z = self.normal_sample();
                    let diffusion = current_vol.to_dec() * sqrt_dt.to_dec() * z;
                    let drift_term = drift * dt.to_dec();

                    let price_change = drift_term + diffusion;
                    price *= price_change.exp();

                    values.push(Positive::new_decimal(price).unwrap_or(Positive::ZERO));
                    vols.push(current_vol);
                }

                Ok(WalkPath {
                    prices: values,
                    vols: Some(vols),
                })
            }
            _ => Err(SimulationError::InvalidWalkType {
                expected: "Telegraph",
            }),
        }
    }
}

/// `Box<dyn WalkTypeAble>` requires `Clone` (via the blanket
/// `WalkTypeAbleClone` impl). Clones share the same RNG stream (`StdRng` is
/// not `Clone` in rand 0.10), so every draw — from the original or any
/// clone — advances one deterministic sequence fixed by the seed.
impl Clone for Walker {
    fn clone(&self) -> Self {
        Walker {
            rng: Arc::clone(&self.rng),
        }
    }
}

/// Re-implementation of every stochastic method of `WalkTypeAble` so samples
/// come from the walker's own (optionally seeded) RNG. The math replicates
/// optionstratlib's default implementations and public kernels; only the
/// source of randomness changes. The `*_with_vol` variants are overridden
/// too because the walk generators consume `generate_with_vol`. `historical`
/// keeps the default implementation as it draws no random numbers.
impl WalkTypeAble<Positive, OptionChain> for Walker {
    fn brownian(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        match params.walk_type {
            WalkType::Brownian {
                dt,
                drift,
                volatility,
            } => {
                let mut values = Vec::with_capacity(params.size + 1);
                let start: Positive = params.ystep_as_positive()?;
                values.push(start);
                let mut x: Decimal = start.to_dec();
                let sigma_abs = (volatility * start).to_dec();
                let sqrt_dt = dt.to_f64().sqrt();
                let sqrt_dt_dec = Decimal::from_f64(sqrt_dt).ok_or_else(|| {
                    SimulationError::non_finite("simulation::brownian::sqrt_dt", sqrt_dt)
                })?;

                for _ in 1..params.size {
                    let z = self.normal_sample();
                    let diffusion = sigma_abs * sqrt_dt_dec * z;
                    let drift_term = drift * dt;
                    x += drift_term + diffusion;
                    values.push(
                        Positive::new_decimal(x.max(Decimal::ZERO)).unwrap_or(Positive::ZERO),
                    );
                }

                Ok(values)
            }
            _ => Err(SimulationError::InvalidWalkType {
                expected: "Brownian",
            }),
        }
    }

    fn geometric_brownian(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        match params.walk_type {
            WalkType::GeometricBrownian {
                dt,
                drift,
                volatility,
            } => {
                let mut values = Vec::with_capacity(params.size);
                let mut current_value: Positive = params.ystep_as_positive()?;
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
            _ => Err(SimulationError::InvalidWalkType {
                expected: "GeometricBrownian",
            }),
        }
    }

    fn log_returns(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        match params.walk_type {
            WalkType::LogReturns {
                dt,
                expected_return,
                volatility,
                autocorrelation,
            } => {
                let mut values = Vec::with_capacity(params.size + 1);
                let mut price: Positive = params.ystep_as_positive()?;
                values.push(price);

                let sqrt_dt = dt.to_f64().sqrt();
                let sqrt_dt_dec = Decimal::from_f64(sqrt_dt).ok_or_else(|| {
                    SimulationError::non_finite("simulation::log_returns::sqrt_dt", sqrt_dt)
                })?;
                let mut prev_log_ret = Decimal::ZERO;

                for _ in 1..params.size {
                    let z = self.normal_sample();
                    let diffusion = z * volatility * sqrt_dt_dec;
                    let mut log_ret = (expected_return * dt) + diffusion;

                    if let Some(ac) = autocorrelation {
                        if !(-Decimal::ONE..=Decimal::ONE).contains(&ac) {
                            return Err(SimulationError::InvalidAutocorrelation { value: ac });
                        }
                        log_ret += ac * prev_log_ret;
                    }

                    price *= log_ret.exp();
                    values.push(price);

                    prev_log_ret = log_ret;
                }
                Ok(values)
            }
            _ => Err(SimulationError::InvalidWalkType {
                expected: "LogReturns",
            }),
        }
    }

    fn mean_reverting(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        match params.walk_type {
            WalkType::MeanReverting {
                dt,
                volatility,
                speed,
                mean,
            } => {
                let sigma_abs = volatility * mean;
                Ok(self.ou_process(
                    params.ystep_as_positive()?,
                    mean,
                    speed,
                    sigma_abs,
                    dt,
                    params.size,
                ))
            }
            _ => Err(SimulationError::InvalidWalkType {
                expected: "MeanReverting",
            }),
        }
    }

    fn jump_diffusion(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
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
                let mut x: Decimal = params.ystep_as_positive()?.to_dec();
                values.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));

                let sqrt_dt = dt.sqrt();
                let lambda_dt = intensity * dt;

                for _ in 1..params.size {
                    let z = self.normal_sample();
                    let sigma_abs = volatility.to_dec() * x;
                    let diffusion = sigma_abs * sqrt_dt.to_dec() * z;

                    let drift_term = drift * dt;
                    let jump = if self.normal_sample() < lambda_dt.to_dec() {
                        // Bernoulli(λdt)
                        jump_mean + self.normal_sample() * jump_volatility
                    } else {
                        Decimal::ZERO
                    };

                    x += drift_term + diffusion + jump;
                    x = x.max(Decimal::ZERO);
                    values.push(Positive::new_decimal(x).unwrap_or(Positive::ZERO));
                }

                Ok(values)
            }
            _ => Err(SimulationError::InvalidWalkType {
                expected: "JumpDiffusion",
            }),
        }
    }

    fn garch(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        Ok(self.garch_walk_seeded(params)?.prices)
    }

    fn garch_with_vol(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
        self.garch_walk_seeded(params)
    }

    fn heston(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        Ok(self.heston_walk_seeded(params)?.prices)
    }

    fn heston_with_vol(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
        self.heston_walk_seeded(params)
    }

    fn custom(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        Ok(self.custom_walk_seeded(params)?.prices)
    }

    fn custom_with_vol(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
        self.custom_walk_seeded(params)
    }

    fn telegraph(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<Vec<Positive>, SimulationError> {
        Ok(self.telegraph_walk_seeded(params)?.prices)
    }

    fn telegraph_with_vol(
        &self,
        params: &WalkParams<Positive, OptionChain>,
    ) -> Result<WalkPath, SimulationError> {
        self.telegraph_walk_seeded(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use positive::pos_or_panic;

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
    fn test_cloned_walker_shares_the_seeded_stream() {
        // Draws interleaved across a walker and its clone must equal the
        // straight sequence of an independent walker with the same seed
        let a = Walker::new_with_seed(7);
        let b = a.clone();
        let mut interleaved = sample_series(&a, 50);
        interleaved.extend(sample_series(&b, 50));

        let reference = Walker::new_with_seed(7);
        assert_eq!(interleaved, sample_series(&reference, 100));
    }

    #[test]
    fn test_seeded_ou_process_is_reproducible() {
        let a = Walker::new_with_seed(7);
        let b = Walker::new_with_seed(7);
        let pa = a.ou_process(
            pos_or_panic!(100.0),
            pos_or_panic!(100.0),
            pos_or_panic!(0.5),
            pos_or_panic!(0.2),
            pos_or_panic!(0.01),
            50,
        );
        let pb = b.ou_process(
            pos_or_panic!(100.0),
            pos_or_panic!(100.0),
            pos_or_panic!(0.5),
            pos_or_panic!(0.2),
            pos_or_panic!(0.01),
            50,
        );
        assert_eq!(pa, pb);
    }
}
