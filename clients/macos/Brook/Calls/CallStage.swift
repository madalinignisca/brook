// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import Foundation

/// Which tile fills the call window. A shared screen is what everyone looks at and is unreadable in
/// a grid cell, so it takes the stage on its own; the user can put any tile on the stage by
/// clicking it. Everything else waits in a strip. No stage means the equal grid, as before.
enum CallStage {
    struct Split {
        let stage: CallModel.Tile?
        let strip: [CallModel.Tile]
    }

    /// `pinned`: the tile the user chose. A pin that is no longer in the call (the person left, the
    /// screen share ended) is ignored, so the layout falls back by itself.
    static func split(_ tiles: [CallModel.Tile], pinned: String?) -> Split {
        let chosen = tiles.first { $0.id == pinned }
            ?? tiles.first { $0.isScreen }
        guard let chosen else { return Split(stage: nil, strip: tiles) }
        return Split(stage: chosen, strip: tiles.filter { $0.id != chosen.id })
    }

    /// The pin to keep after the tiles changed: one whose tile is gone is dropped for good, so it
    /// cannot come back when the same id appears again (the same person shares their screen again).
    static func pruned(_ pinned: String?, tiles: [CallModel.Tile]) -> String? {
        guard let pinned, tiles.contains(where: { $0.id == pinned }) else { return nil }
        return pinned
    }

    /// Who is sharing, in the order the shares arrived: those still sharing keep their place, a new
    /// share goes last (several in one update by participant id, so the order is the same every run).
    /// The first share on the stage is then the one everyone was already watching, whoever comes
    /// earlier in the roster.
    static func arrivalOrder(previous: [String], current: [String]) -> [String] {
        let now = Set(current)
        let kept = previous.filter { now.contains($0) }
        let known = Set(kept)
        return kept + current.filter { !known.contains($0) }.sorted()
    }

    /// A click on a tile: on the stage it releases the pin (back to the automatic choice), anywhere
    /// else it pins that tile.
    static func toggled(_ pinned: String?, clicked id: String, onStage: String?) -> String? {
        id == onStage && pinned == id ? nil : id
    }
}
