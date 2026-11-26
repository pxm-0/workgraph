# List items

## Sub-features
- An empty list prints nothing and exits 0.

## How to get to it (user POV)
Add two items, then run `todo list`.

## Driving it with sh
`./todo add first; ./todo add second; ./todo list`, all with the same `TODO_FILE`.

**Proof:** the output is `1. first` then `2. second`, in that order.

## Gotchas
- Numbering is by line in the data file, so the journey needs its own fresh file.
