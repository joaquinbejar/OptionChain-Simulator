use optionstratlib::Positive;
use optionstratlib::chains::OptionChain;
use optionstratlib::simulation::WalkTypeAble;

/// Walker struct for implementing WalkTypeAble
pub(crate) struct Walker {}

impl Walker {
    pub(crate) fn new() -> Self {
        Walker {}
    }
}

impl WalkTypeAble<Positive, OptionChain> for Walker {}
