# Add an item

## Sub-features
- `todo add <text>` with several words keeps them as one item.

## How to get to it (user POV)
Run `todo add buy milk` in a terminal.

## Driving it with sh
`TODO_FILE=<scratch>/add.txt ./todo add buy milk`

**Proof:** the CLI prints `added: buy milk`, exits 0, and the data file holds the line `buy milk`.

## Gotchas
- `todo add` with no text exits 2 with a usage line; that is not this journey.
