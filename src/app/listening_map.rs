use std::collections::BTreeSet;

use crate::atlas::{geometry::world, mask::LandMask};
use crate::personal_state::PersonalStateV2;

#[derive(Default)]
pub struct PassportMap {
    revision: Option<u64>,
    pub countries: BTreeSet<[u8; 2]>,
    pub mask: Option<LandMask>,
}

impl PassportMap {
    pub fn update(&mut self, state: &PersonalStateV2) {
        if self.revision == Some(state.revision) {
            return;
        }
        let Ok(projection) = crate::listening::ListeningProjection::from_ledger(state) else {
            return;
        };
        let countries = projection
            .passport_visits
            .values()
            .filter_map(|visit| {
                let code = visit.country_code.as_ref()?.as_bytes();
                (code.len() == 2).then(|| [code[0], code[1]])
            })
            .collect::<BTreeSet<_>>();
        if countries != self.countries {
            self.mask = (!countries.is_empty()).then(|| {
                LandMask::build_countries(
                    world()
                        .countries()
                        .iter()
                        .filter(|country| countries.contains(&country.code)),
                )
            });
            self.countries = countries;
        }
        self.revision = Some(state.revision);
    }
}
