//! Merged water surfaces, a Switch optimization (the port's OPTIMIZATIONS.md O20): a section
//! whose see-through quads all lie on one horizontal plane as whole-block faces showing their
//! sprite whole, an open lake's or the sea's surface and its underside, draws each row's runs of
//! like faces (sprite, light, tint) as strips a block deep, the sprite repeated once a block
//! (`TILED_LAYER`). An open 16×16 surface takes 32 quads a side instead of 256.
//!
//! No edge ends partway along another (a T-junction, whose rasterized edges can leave a pixel
//! between them uncovered or blend it twice): the line between two rows has a vertex wherever a
//! run of either row ends, and both rows' strips take every one of them; a section's edge lines
//! have one at every block, as a neighbour's unmerged faces do. Any other see-through quad keeps
//! the section as it is: one large face sorted against another surface could hide what lies
//! behind it, as the translucent pass writes depth.
use super::{TILED_LAYER, TerrainVertex, UV_STEPS};

/// What faces share to join a strip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Key {
    layer: u16,
    light: u16,
    color: u32,
}

/// A whole-block face's corners from its cell's (x, z), in fluid.rs's order: the top's nw, sw,
/// se, ne, and the underside's nw, ne, se, sw.
const CORNERS: [[[u8; 2]; 4]; 2] = [
    [[0, 0], [0, 1], [1, 1], [1, 0]],
    [[0, 0], [1, 0], [1, 1], [0, 1]],
];

/// The cell (x, z), side (0 the top, 1 the underside) and key of a whole-block face at height `y`
/// showing its sprite whole, or None.
fn face(quad: &[TerrainVertex], y: f32) -> Option<([usize; 2], usize, Key)> {
    let first = quad[0];
    let [x, _, z] = first.pos;
    if first.layer & TILED_LAYER != 0
        || !(0.0..16.0).contains(&x)
        || !(0.0..16.0).contains(&z)
        || x.fract() != 0.0
        || z.fract() != 0.0
    {
        return None;
    }
    let whole = UV_STEPS as u16;
    let side = CORNERS.iter().position(|corners| {
        quad.iter().zip(corners).all(|(v, &[dx, dz])| {
            v.pos == [x + f32::from(dx), y, z + f32::from(dz)]
                && v.uv == [u16::from(dx) * whole, u16::from(dz) * whole]
                && (v.layer, v.light, v.color) == (first.layer, first.light, first.color)
        })
    })?;
    let key = Key {
        layer: first.layer,
        light: first.light,
        color: first.color,
    };
    Some(([x as usize, z as usize], side, key))
}

/// Merges `translucent`'s quads into strips when they are one plane's whole faces (the module's
/// doc); false, leaving them as they are, otherwise.
pub(super) fn merge(translucent: &mut Vec<TerrainVertex>) -> bool {
    let Some(y) = translucent.first().map(|v| v.pos[1]) else {
        return false;
    };
    if translucent.len() % 4 != 0 {
        return false;
    }
    // cells[side][z][x]
    let mut cells = [[[None; 16]; 16]; 2];
    for quad in translucent.chunks_exact(4) {
        let Some(([x, z], side, key)) = face(quad, y) else {
            return false;
        };
        if cells[side][z][x].replace(key).is_some() {
            return false;
        }
    }
    let mut merged = Vec::new();
    for (side, rows) in cells.iter().enumerate() {
        strips(rows, side == 1, y, &mut merged);
    }
    if merged.len() >= translucent.len() {
        return false;
    }
    *translucent = merged;
    true
}

/// Bit x (0 to 16) set where a run of like faces along `row` starts or ends.
fn ends(row: &[Option<Key>; 16]) -> u32 {
    let mut bits = 0;
    let mut before = None;
    for x in 0..=16 {
        let at = row.get(x).copied().flatten();
        if at != before {
            bits |= 1 << x;
        }
        before = at;
    }
    bits
}

/// A section's edge line: a vertex at every block, where a neighbour's faces have theirs.
const EVERY_BLOCK: u32 = (1 << 17) - 1;

/// One side's strips: each row's runs of like faces, between the vertices of the lines either
/// side of the row.
fn strips(rows: &[[Option<Key>; 16]; 16], underside: bool, y: f32, out: &mut Vec<TerrainVertex>) {
    let ends: [u32; 16] = std::array::from_fn(|z| ends(&rows[z]));
    // Line z runs between rows z - 1 and z.
    let line = |z: usize| match z {
        0 | 16 => EVERY_BLOCK,
        _ => ends[z - 1] | ends[z],
    };
    for (z, row) in rows.iter().enumerate() {
        let mut x = 0;
        while x < 16 {
            let Some(key) = row[x] else {
                x += 1;
                continue;
            };
            let start = x;
            while x < 16 && row[x] == Some(key) {
                x += 1;
            }
            let points = |line: u32| -> Vec<usize> {
                (start..=x).filter(|&p| line & 1 << p != 0).collect()
            };
            let strip = Strip {
                start,
                z,
                y,
                key,
                underside,
            };
            strip.emit(&points(line(z)), &points(line(z + 1)), out);
        }
    }
}

/// A run of like faces along row `z`, from x = `start`.
struct Strip {
    start: usize,
    z: usize,
    y: f32,
    key: Key,
    underside: bool,
}

impl Strip {
    /// A corner: its x, and 0 on the strip's line z or 1 on line z + 1.
    fn vertex(&self, [x, dz]: [usize; 2]) -> TerrainVertex {
        TerrainVertex::tiled(
            [x as f32, self.y, (self.z + dz) as f32],
            [(x - self.start) as f32, dz as f32],
            self.key.layer | TILED_LAYER,
            self.key.color,
            self.key.light,
        )
    }

    /// The ladder between the strip's points on `bottom` (line z) and `top` (line z + 1), both
    /// from its start to its end: each triangle, wound as the top's corners, takes the next point
    /// of the line whose new rung is the shorter; two in turn share a rung, the diagonal (corners
    /// 0 and 2) of the quad they make.
    fn emit(&self, bottom: &[usize], top: &[usize], out: &mut Vec<TerrainVertex>) {
        let mut quad = |q: [[usize; 2]; 4]| {
            let q = if self.underside {
                [q[0], q[3], q[2], q[1]]
            } else {
                q
            };
            out.extend(q.map(|corner| self.vertex(corner)));
        };
        let (mut i, mut j) = (0, 0);
        let mut pending: Option<[[usize; 2]; 3]> = None;
        while i + 1 < bottom.len() || j + 1 < top.len() {
            let along_bottom = j + 1 == top.len()
                || i + 1 < bottom.len()
                    && bottom[i + 1].abs_diff(top[j]) <= bottom[i].abs_diff(top[j + 1]);
            let triangle = if along_bottom {
                i += 1;
                [[bottom[i - 1], 0], [top[j], 1], [bottom[i], 0]]
            } else {
                j += 1;
                [[bottom[i], 0], [top[j - 1], 1], [top[j], 1]]
            };
            match pending.take() {
                None => pending = Some(triangle),
                // After a step along the bottom, the rung is the first triangle's last corner and
                // its second.
                Some(first) if first[2][1] == 0 => {
                    quad([first[2], first[0], first[1], triangle[2]]);
                }
                Some(first) => quad([first[0], first[1], first[2], triangle[2]]),
            }
        }
        if let Some([p, q, r]) = pending {
            quad([p, q, r, r]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesher::TILED_UV_STEPS;

    /// The quad fluid.rs makes for the face of cell (x, z) at height `y`.
    fn water(x: usize, z: usize, y: f32, underside: bool, light: u16, color: u32) -> [TerrainVertex; 4] {
        CORNERS[usize::from(underside)].map(|[dx, dz]| {
            TerrainVertex::new(
                [(x + usize::from(dx)) as f32, y, (z + usize::from(dz)) as f32],
                [f32::from(dx), f32::from(dz)],
                7,
                color,
                light,
            )
        })
    }

    /// A lake's surface: open water, a shore along a diagonal and a cove, a row in the shade of a
    /// tree (other light), a strip of another biome's tint, and the underside everywhere but under
    /// a dock (fluid.rs draws it where a cell above is open).
    fn lake() -> Vec<TerrainVertex> {
        let y = 15.0 - 1.0 / 9.0 - 0.001;
        let mut quads = Vec::new();
        for z in 0..16 {
            for x in 0..16 {
                if x + z < 6 || (9..12).contains(&x) && (4..7).contains(&z) {
                    continue;
                }
                let light = if z == 10 && x < 8 { 0x00B0 } else { 0x00F0 };
                let color = if x >= 13 { 0xFF3F_76E4 } else { 0xFF44_AFF5 };
                quads.extend(water(x, z, y, false, light, color));
                if !((2..6).contains(&x) && (12..16).contains(&z)) {
                    quads.extend(water(x, z, y, true, light, color));
                }
            }
        }
        quads
    }

    /// Triangles as (corners in (x, z), the vertex they came from), degenerate ones left out.
    fn triangles(vertices: &[TerrainVertex]) -> Vec<[([i64; 2], TerrainVertex); 3]> {
        vertices
            .chunks_exact(4)
            .flat_map(|q| [[q[0], q[1], q[2]], [q[2], q[3], q[0]]])
            .map(|t| t.map(|v| ([v.pos[0] as i64, v.pos[2] as i64], v)))
            .filter(|t| t[0].0 != t[1].0 && t[1].0 != t[2].0 && t[2].0 != t[0].0)
            .collect()
    }

    /// Twice the signed area in the (x, z) plane: negative as the top face winds.
    fn area2([a, b, c]: [[i64; 2]; 3]) -> i64 {
        (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])
    }

    #[test]
    fn an_open_surface_takes_32_quads_a_side() {
        let y = 7.0;
        let mut quads: Vec<_> = (0..256)
            .flat_map(|i| [false, true].map(|under| water(i % 16, i / 16, y, under, 0xF0, !0)))
            .flatten()
            .collect();
        assert!(merge(&mut quads));
        assert_eq!(quads.len() / 4, 64);
    }

    /// Merged, the triangles cover each cell's area once, with its sprite, light and tint, the
    /// sprite repeated once a block from the cell's corner as before; and no vertex lies inside
    /// another triangle's edge.
    #[test]
    fn the_merged_lake_covers_each_cell_once_without_t_junctions() {
        let original = lake();
        let mut merged = original.clone();
        assert!(merge(&mut merged));
        assert!(merged.len() * 2 < original.len(), "{} of {}", merged.len(), original.len());
        for (side, underside) in [(0, false), (1, true)] {
            let tris: Vec<_> = triangles(&merged)
                .into_iter()
                .filter(|t| (area2(t.map(|c| c.0)) > 0) == underside)
                .collect();
            let cells: Vec<_> = original
                .chunks_exact(4)
                .filter_map(|q| face(q, original[0].pos[1]))
                .filter(|f| f.1 == side)
                .collect();
            let key_at = |x: i64, z: i64| {
                cells.iter().find(|c| c.0 == [x as usize, z as usize]).map(|c| c.2)
            };
            // The same area, and every triangle within a cell row of its own key: no overlaps
            // and nothing outside.
            let area: i64 = tris.iter().map(|t| area2(t.map(|c| c.0)).abs()).sum();
            assert_eq!(area, 2 * cells.len() as i64, "side {side}");
            for t in &tris {
                let [x3, z3] = [0, 1].map(|i| t.iter().map(|c| c.0[i]).sum::<i64>());
                let v = t[0].1;
                let key = Key {
                    layer: v.layer & !TILED_LAYER,
                    light: v.light,
                    color: v.color,
                };
                assert_eq!(key_at(x3.div_euclid(3), z3.div_euclid(3)), Some(key), "side {side}");
            }
            for ([x, z], _, key) in &cells {
                // A point in the cell no rung crosses, (x + 1/4, z + 1/2), in quarters of a
                // block to stay in integers.
                let p = [4 * *x as i64 + 1, 4 * *z as i64 + 2];
                let inside: Vec<_> = tris
                    .iter()
                    .filter(|t| {
                        let c = t.map(|c| [4 * c.0[0], 4 * c.0[1]]);
                        let s = [area2([c[0], c[1], p]), area2([c[1], c[2], p]), area2([c[2], c[0], p])];
                        s.iter().all(|&s| s < 0) || s.iter().all(|&s| s > 0)
                    })
                    .collect();
                assert_eq!(inside.len(), 1, "side {side} cell {x} {z}");
                for (corner, v) in inside[0] {
                    assert_eq!(v.layer, key.layer | TILED_LAYER);
                    assert_eq!((v.light, v.color), (key.light, key.color));
                    // The UV is the corner's offset from the cell's, in whole blocks.
                    let uv = v.uv.map(|s| f32::from(s) / TILED_UV_STEPS);
                    let du = uv[0] - (corner[0] - *x as i64) as f32;
                    let dv = uv[1] - (corner[1] - *z as i64) as f32;
                    assert!(du.fract() == 0.0 && dv == 0.0, "side {side} cell {x} {z}: {uv:?}");
                }
            }
            let corners: Vec<[i64; 2]> = tris.iter().flat_map(|t| t.map(|c| c.0)).collect();
            for t in &tris {
                for (a, b) in [(t[0].0, t[1].0), (t[1].0, t[2].0), (t[2].0, t[0].0)] {
                    let d = [b[0] - a[0], b[1] - a[1]];
                    let length2 = d[0] * d[0] + d[1] * d[1];
                    for v in &corners {
                        let e = [v[0] - a[0], v[1] - a[1]];
                        let along = e[0] * d[0] + e[1] * d[1];
                        assert!(
                            e[0] * d[1] - e[1] * d[0] != 0 || along <= 0 || along >= length2,
                            "side {side}: {v:?} inside edge {a:?}-{b:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn other_see_through_quads_keep_the_section() {
        let y = 4.0 - 1.0 / 9.0 - 0.001;
        let open: Vec<_> = (0..64).flat_map(|i| water(i % 16, i / 16, y, false, 0xF0, !0)).collect();
        // A second surface, a block down.
        let mut two = open.clone();
        two.extend(water(3, 9, y - 1.0, false, 0xF0, !0));
        // A flowing top beside it: its UVs turn on its corners.
        let mut flowing = open.clone();
        let mut turned = water(3, 9, y, false, 0xF0, !0);
        let uvs = turned.map(|v| v.uv);
        for (i, v) in turned.iter_mut().enumerate() {
            v.uv = uvs[(i + 1) % 4];
        }
        flowing.extend(turned);
        for mut quads in [two, flowing] {
            let before = quads.clone();
            assert!(!merge(&mut quads));
            assert_eq!(bytemuck::cast_slice::<_, u8>(&quads), bytemuck::cast_slice::<_, u8>(&before));
        }
    }
}
