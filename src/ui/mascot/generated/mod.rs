pub mod cat_laptop;
pub mod dj_gem;

static ALL_ASSETS: [&super::asset::MascotAsset; 10] = [
    &dj_gem::DJ_GEM_IDLE,
    &dj_gem::DJ_GEM_IDLE_RETRO,
    &dj_gem::DJ_GEM_GROOVE,
    &dj_gem::DJ_GEM_GROOVE_RETRO,
    &dj_gem::DJ_GEM_THINKING,
    &dj_gem::DJ_GEM_THINKING_RETRO,
    &cat_laptop::CAT_LAPTOP_IDLE,
    &cat_laptop::CAT_LAPTOP_IDLE_RETRO,
    &cat_laptop::CAT_LAPTOP_GROOVE,
    &cat_laptop::CAT_LAPTOP_GROOVE_RETRO,
];

pub fn all_assets() -> &'static [&'static super::asset::MascotAsset] {
    &ALL_ASSETS
}
