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

    /// A click on a tile: on the stage it releases the pin (back to the automatic choice), anywhere
    /// else it pins that tile.
    static func toggled(_ pinned: String?, clicked id: String, onStage: String?) -> String? {
        id == onStage && pinned == id ? nil : id
    }
}
