# Celestial logo

The canonical artwork is `assets/com.unboundmark.Celestial-regular.svg`: two synchronization arrows orbiting a four-point star. It uses the UI's accent teal (`#2fb3a3`), bright teal (`#3fd4c1`) and white (`#ffffff`). The background is transparent. All artwork is editable SVG paths; there are no embedded images, fonts, gradients or filters.

The sidebar loads the embedded SVG. Qt renders the same source for window icons and the tray's 32/64-pixel ARGB pixmaps. The tray adds a status badge using the existing status colors. Desktop installation and native/portable staging use the same SVG. Existing menu/autostart installations refresh their icon when the updated application starts.

## Design provenance

The concept was generated with the built-in imagegen tool, then recreated as three SVG paths for the shipped asset. Generation prompt:

> Use case: logo-brand. Create one new application logo for Celestial, a Linux cloud folder synchronization application. Flat vector-style geometric mark, transparent background. A bold circular C-shaped orbit made from two smoothly curved synchronization arrows surrounds a single compact four-point celestial star. Opening in the circular orbit toward upper-right. Strong silhouette readable at 24px. Colors strictly #2fb3a3 teal, #3fd4c1 bright teal, and #ffffff star; these match the application's charcoal dark UI. Centered square composition with comfortable padding. No lettering, no gradients, no shadows, no texture, no mockup, no background tile, no decorative extra stars. Minimal simple SVG-friendly shapes.
