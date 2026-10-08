use nvim_oxi::{Dictionary, Object, Result};

pub mod vimregex;

#[nvim_oxi::plugin]
fn matchup_rs() -> Result<Dictionary> {
    Ok(Dictionary::from_iter([(
        "version",
        Object::from(env!("CARGO_PKG_VERSION")),
    )]))
}
