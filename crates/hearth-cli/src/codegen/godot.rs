//! GDScript generation.
//!
//! This is a minimal stub: it emits one file per room so the `hearth codegen`
//! driver can be tested end to end. Plan Task 7 replaces it with the real
//! typed-class generator; the driver only depends on `generate`'s signature.

use hearth_server::SchemaDoc;

use crate::codegen::GENERATED_HEADER;

/// Render one GDScript file per room: `<Room><StateRoot>` in snake case.
pub fn generate(docs: &[SchemaDoc]) -> Result<Vec<(String, String)>, String> {
    let files = docs
        .iter()
        .map(|doc| {
            let class = format!("{}{}", doc.room, doc.state);
            let file = format!("{}.gd", snake_case(&class));
            let contents = format!(
                "{GENERATED_HEADER}\n# placeholder for {class} (schema hash {})\n",
                doc.hash
            );
            (file, contents)
        })
        .collect();
    Ok(files)
}

/// `TicTacToeBoard` becomes `tic_tac_toe_board`.
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (index, character) in name.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}
