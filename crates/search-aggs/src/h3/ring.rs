//! Neighbors in the H3 grid (`HexRing.h3NeighborInDirection`, from Uber's H3, Apache-2.0, as
//! Elasticsearch ports it): digits are rewritten from the finest resolution up, with the
//! class II / class III digit and carry tables, until no carry is left or the base cell
//! changes; base-cell crossings rotate the digits, and pentagons skip their deleted k
//! sub-sequence. Also the hierarchy helpers `geohex_grid` on shapes walks: children, the
//! neighbouring cells that overlap a cell without being its children, rings, parents.

use super::base_cells::BASE_CELL_DATA;
use super::core::{
    H3_INIT, base_cell_is_cw_offset, get_base_cell, get_digit, get_resolution, is_base_cell_pentagon, is_pentagon, leading_non_zero_digit,
    rotate_pent60ccw, rotate60ccw, rotate60cw, set_digit,
};

const INVALID_BASE_CELL: i32 = 127;
const CENTER: i32 = 0;
const K_AXES: i32 = 1;
const JK_AXES: i32 = 3;
const IK_AXES: i32 = 5;

/// `baseCellNeighbors`: per base cell and direction.
const BASE_CELL_NEIGHBORS: [[u8; 7]; 122] = [
    [0, 1, 5, 2, 4, 3, 8],
    [1, 7, 6, 9, 0, 3, 2],
    [2, 6, 10, 11, 0, 1, 5],
    [3, 13, 1, 7, 4, 12, 0],
    [4, 127, 15, 8, 3, 0, 12],
    [5, 2, 18, 10, 8, 0, 16],
    [6, 14, 11, 17, 1, 9, 2],
    [7, 21, 9, 19, 3, 13, 1],
    [8, 5, 22, 16, 4, 0, 15],
    [9, 19, 14, 20, 1, 7, 6],
    [10, 11, 24, 23, 5, 2, 18],
    [11, 17, 23, 25, 2, 6, 10],
    [12, 28, 13, 26, 4, 15, 3],
    [13, 26, 21, 29, 3, 12, 7],
    [14, 127, 17, 27, 9, 20, 6],
    [15, 22, 28, 31, 4, 8, 12],
    [16, 18, 33, 30, 8, 5, 22],
    [17, 11, 14, 6, 35, 25, 27],
    [18, 24, 30, 32, 5, 10, 16],
    [19, 34, 20, 36, 7, 21, 9],
    [20, 14, 19, 9, 40, 27, 36],
    [21, 38, 19, 34, 13, 29, 7],
    [22, 16, 41, 33, 15, 8, 31],
    [23, 24, 11, 10, 39, 37, 25],
    [24, 127, 32, 37, 10, 23, 18],
    [25, 23, 17, 11, 45, 39, 35],
    [26, 42, 29, 43, 12, 28, 13],
    [27, 40, 35, 46, 14, 20, 17],
    [28, 31, 42, 44, 12, 15, 26],
    [29, 43, 38, 47, 13, 26, 21],
    [30, 32, 48, 50, 16, 18, 33],
    [31, 41, 44, 53, 15, 22, 28],
    [32, 30, 24, 18, 52, 50, 37],
    [33, 30, 49, 48, 22, 16, 41],
    [34, 19, 38, 21, 54, 36, 51],
    [35, 46, 45, 56, 17, 27, 25],
    [36, 20, 34, 19, 55, 40, 54],
    [37, 39, 52, 57, 24, 23, 32],
    [38, 127, 34, 51, 29, 47, 21],
    [39, 37, 25, 23, 59, 57, 45],
    [40, 27, 36, 20, 60, 46, 55],
    [41, 49, 53, 61, 22, 33, 31],
    [42, 58, 43, 62, 28, 44, 26],
    [43, 62, 47, 64, 26, 42, 29],
    [44, 53, 58, 65, 28, 31, 42],
    [45, 39, 35, 25, 63, 59, 56],
    [46, 60, 56, 68, 27, 40, 35],
    [47, 38, 43, 29, 69, 51, 64],
    [48, 49, 30, 33, 67, 66, 50],
    [49, 127, 61, 66, 33, 48, 41],
    [50, 48, 32, 30, 70, 67, 52],
    [51, 69, 54, 71, 38, 47, 34],
    [52, 57, 70, 74, 32, 37, 50],
    [53, 61, 65, 75, 31, 41, 44],
    [54, 71, 55, 73, 34, 51, 36],
    [55, 40, 54, 36, 72, 60, 73],
    [56, 68, 63, 77, 35, 46, 45],
    [57, 59, 74, 78, 37, 39, 52],
    [58, 127, 62, 76, 44, 65, 42],
    [59, 63, 78, 79, 39, 45, 57],
    [60, 72, 68, 80, 40, 55, 46],
    [61, 53, 49, 41, 81, 75, 66],
    [62, 43, 58, 42, 82, 64, 76],
    [63, 127, 56, 45, 79, 59, 77],
    [64, 47, 62, 43, 84, 69, 82],
    [65, 58, 53, 44, 86, 76, 75],
    [66, 67, 81, 85, 49, 48, 61],
    [67, 66, 50, 48, 87, 85, 70],
    [68, 56, 60, 46, 90, 77, 80],
    [69, 51, 64, 47, 89, 71, 84],
    [70, 67, 52, 50, 83, 87, 74],
    [71, 89, 73, 91, 51, 69, 54],
    [72, 127, 73, 55, 80, 60, 88],
    [73, 91, 72, 88, 54, 71, 55],
    [74, 78, 83, 92, 52, 57, 70],
    [75, 65, 61, 53, 94, 86, 81],
    [76, 86, 82, 96, 58, 65, 62],
    [77, 63, 68, 56, 93, 79, 90],
    [78, 74, 59, 57, 95, 92, 79],
    [79, 78, 63, 59, 93, 95, 77],
    [80, 68, 72, 60, 99, 90, 88],
    [81, 85, 94, 101, 61, 66, 75],
    [82, 96, 84, 98, 62, 76, 64],
    [83, 127, 74, 70, 100, 87, 92],
    [84, 69, 82, 64, 97, 89, 98],
    [85, 87, 101, 102, 66, 67, 81],
    [86, 76, 75, 65, 104, 96, 94],
    [87, 83, 102, 100, 67, 70, 85],
    [88, 72, 91, 73, 99, 80, 105],
    [89, 97, 91, 103, 69, 84, 71],
    [90, 77, 80, 68, 106, 93, 99],
    [91, 73, 89, 71, 105, 88, 103],
    [92, 83, 78, 74, 108, 100, 95],
    [93, 79, 90, 77, 109, 95, 106],
    [94, 86, 81, 75, 107, 104, 101],
    [95, 92, 79, 78, 109, 108, 93],
    [96, 104, 98, 110, 76, 86, 82],
    [97, 127, 98, 84, 103, 89, 111],
    [98, 110, 97, 111, 82, 96, 84],
    [99, 80, 105, 88, 106, 90, 113],
    [100, 102, 83, 87, 108, 114, 92],
    [101, 102, 107, 112, 81, 85, 94],
    [102, 101, 87, 85, 114, 112, 100],
    [103, 91, 97, 89, 116, 105, 111],
    [104, 107, 110, 115, 86, 94, 96],
    [105, 88, 103, 91, 113, 99, 116],
    [106, 93, 99, 90, 117, 109, 113],
    [107, 127, 101, 94, 115, 104, 112],
    [108, 100, 95, 92, 118, 114, 109],
    [109, 108, 93, 95, 117, 118, 106],
    [110, 98, 104, 96, 119, 111, 115],
    [111, 97, 110, 98, 116, 103, 119],
    [112, 107, 102, 101, 120, 115, 114],
    [113, 99, 116, 105, 117, 106, 121],
    [114, 112, 100, 102, 118, 120, 108],
    [115, 110, 107, 104, 120, 119, 112],
    [116, 103, 119, 111, 113, 105, 121],
    [117, 127, 109, 118, 113, 121, 106],
    [118, 120, 108, 114, 117, 121, 109],
    [119, 111, 115, 110, 121, 116, 120],
    [120, 115, 114, 112, 121, 119, 118],
    [121, 116, 120, 119, 117, 113, 118],
];

/// `baseCellNeighbor60CCWRots` (-1: no neighbour that way).
const BASE_CELL_NEIGHBOR_60CCW_ROTS: [[i8; 7]; 122] = [
    [0, 5, 0, 0, 1, 5, 1],
    [0, 0, 1, 0, 1, 0, 1],
    [0, 0, 0, 0, 0, 5, 0],
    [0, 5, 0, 0, 2, 5, 1],
    [0, -1, 1, 0, 3, 4, 2],
    [0, 0, 1, 0, 1, 0, 1],
    [0, 0, 0, 3, 5, 5, 0],
    [0, 0, 0, 0, 0, 5, 0],
    [0, 5, 0, 0, 0, 5, 1],
    [0, 0, 1, 3, 0, 0, 1],
    [0, 0, 1, 3, 0, 0, 1],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 5, 0, 0, 3, 5, 1],
    [0, 0, 1, 0, 1, 0, 1],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 5, 0, 0, 4, 5, 1],
    [0, 0, 0, 0, 0, 5, 0],
    [0, 3, 3, 3, 3, 0, 3],
    [0, 0, 0, 3, 5, 5, 0],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 3, 3, 3, 0, 3, 0],
    [0, 0, 0, 3, 5, 5, 0],
    [0, 0, 1, 0, 1, 0, 1],
    [0, 3, 3, 3, 0, 3, 0],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 0, 0, 3, 0, 0, 3],
    [0, 0, 0, 0, 0, 5, 0],
    [0, 3, 0, 0, 0, 3, 3],
    [0, 0, 1, 0, 1, 0, 1],
    [0, 0, 1, 3, 0, 0, 1],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 0, 0, 0, 5, 0],
    [0, 3, 3, 3, 3, 0, 3],
    [0, 0, 1, 3, 0, 0, 1],
    [0, 3, 3, 3, 3, 0, 3],
    [0, 0, 3, 0, 3, 0, 3],
    [0, 0, 0, 3, 0, 0, 3],
    [0, 3, 0, 0, 0, 3, 3],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 3, 0, 0, 3, 3, 0],
    [0, 3, 0, 0, 3, 3, 0],
    [0, 0, 0, 3, 5, 5, 0],
    [0, 0, 0, 3, 5, 5, 0],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 1, 3, 0, 0, 1],
    [0, 0, 3, 0, 0, 3, 3],
    [0, 0, 0, 3, 0, 3, 0],
    [0, 3, 3, 3, 0, 3, 0],
    [0, 3, 3, 3, 0, 3, 0],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 0, 0, 3, 0, 0, 3],
    [0, 3, 0, 0, 0, 3, 3],
    [0, 0, 3, 0, 3, 0, 3],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 3, 0, 3, 0, 3],
    [0, 0, 3, 0, 0, 3, 3],
    [0, 3, 3, 3, 0, 0, 3],
    [0, 0, 0, 3, 0, 3, 0],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 3, 3, 3, 3, 3, 0],
    [0, 3, 3, 3, 3, 3, 0],
    [0, 3, 3, 3, 3, 0, 3],
    [0, 3, 3, 3, 3, 0, 3],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 0, 0, 3, 0, 0, 3],
    [0, 3, 3, 3, 0, 3, 0],
    [0, 3, 0, 0, 0, 3, 3],
    [0, 3, 0, 0, 3, 3, 0],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 3, 0, 0, 3, 3, 0],
    [0, 0, 3, 0, 0, 3, 3],
    [0, 0, 0, 3, 0, 3, 0],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 3, 3, 3, 0, 0, 3],
    [0, 3, 3, 3, 0, 0, 3],
    [0, 0, 0, 3, 0, 0, 3],
    [0, 3, 0, 0, 0, 3, 3],
    [0, 0, 0, 3, 0, 5, 0],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 1, 3, 1, 0, 1],
    [0, 0, 1, 3, 1, 0, 1],
    [0, 0, 3, 0, 3, 0, 3],
    [0, 0, 3, 0, 3, 0, 3],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 0, 3, 0, 0, 3, 3],
    [0, 0, 0, 3, 0, 3, 0],
    [0, 3, 0, 0, 3, 3, 0],
    [0, 3, 3, 3, 3, 3, 0],
    [0, 0, 0, 3, 0, 5, 0],
    [0, 3, 3, 3, 3, 3, 0],
    [0, 0, 0, 0, 0, 0, 1],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 0, 3, 0, 5, 0],
    [0, 5, 0, 0, 5, 5, 0],
    [0, 0, 3, 0, 0, 3, 3],
    [0, 0, 0, 0, 0, 0, 1],
    [0, 0, 0, 3, 0, 3, 0],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 3, 3, 3, 0, 0, 3],
    [0, 5, 0, 0, 5, 5, 0],
    [0, 0, 1, 3, 1, 0, 1],
    [0, 3, 3, 3, 0, 0, 3],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 1, 3, 1, 0, 1],
    [0, 3, 3, 3, 3, 3, 0],
    [0, 0, 0, 0, 0, 0, 1],
    [0, 0, 1, 0, 3, 5, 1],
    [0, -1, 3, 0, 5, 2, 0],
    [0, 5, 0, 0, 5, 5, 0],
    [0, 0, 1, 0, 4, 5, 1],
    [0, 3, 3, 3, 0, 0, 0],
    [0, 0, 0, 3, 0, 5, 0],
    [0, 0, 0, 3, 0, 5, 0],
    [0, 0, 1, 0, 2, 5, 1],
    [0, 0, 0, 0, 0, 0, 1],
    [0, 0, 1, 3, 1, 0, 1],
    [0, 5, 0, 0, 5, 5, 0],
    [0, -1, 1, 0, 3, 4, 2],
    [0, 0, 1, 0, 0, 5, 1],
    [0, 0, 0, 0, 0, 0, 1],
    [0, 5, 0, 0, 5, 5, 0],
    [0, 0, 1, 0, 1, 5, 1],
];

/// `NEW_DIGIT_II`: digit -> direction -> new digit.
const NEW_DIGIT_II: [[u8; 7]; 7] = [
    [0, 1, 2, 3, 4, 5, 6],
    [1, 4, 3, 6, 5, 2, 0],
    [2, 3, 1, 4, 6, 0, 5],
    [3, 6, 4, 5, 0, 1, 2],
    [4, 5, 6, 0, 2, 3, 1],
    [5, 2, 0, 1, 3, 6, 4],
    [6, 0, 5, 2, 1, 4, 3],
];

/// `NEW_ADJUSTMENT_II`: digit -> direction -> carry direction.
const NEW_ADJUSTMENT_II: [[u8; 7]; 7] = [
    [0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 1, 0, 5, 0],
    [0, 0, 2, 3, 0, 0, 2],
    [0, 1, 3, 3, 0, 0, 0],
    [0, 0, 0, 0, 4, 4, 6],
    [0, 5, 0, 0, 4, 5, 0],
    [0, 0, 2, 0, 6, 0, 6],
];

/// `NEW_DIGIT_III`.
const NEW_DIGIT_III: [[u8; 7]; 7] = [
    [0, 1, 2, 3, 4, 5, 6],
    [1, 2, 3, 4, 5, 6, 0],
    [2, 3, 4, 5, 6, 0, 1],
    [3, 4, 5, 6, 0, 1, 2],
    [4, 5, 6, 0, 1, 2, 3],
    [5, 6, 0, 1, 2, 3, 4],
    [6, 0, 1, 2, 3, 4, 5],
];

/// `NEW_ADJUSTMENT_III`.
const NEW_ADJUSTMENT_III: [[u8; 7]; 7] = [
    [0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 3, 0, 1, 0],
    [0, 0, 2, 2, 0, 0, 6],
    [0, 3, 2, 3, 0, 0, 0],
    [0, 0, 0, 0, 4, 5, 4],
    [0, 1, 0, 0, 5, 5, 0],
    [0, 0, 6, 0, 4, 0, 6],
];

/// `HexRing.DIRECTIONS`: ring positions in order (J, JK, K, IK, I, IJ).
const DIRECTIONS: [i32; 6] = [2, 3, 1, 5, 4, 6];

const H3_MODE_OFFSET: u32 = 59;
const H3_BC_OFFSET: u32 = 45;
const H3_RES_OFFSET: u32 = 52;
/// The finest resolution.
const MAX_RES: i32 = 15;

fn set_base_cell(h: u64, bc: i32) -> u64 {
    (h & !(127u64 << H3_BC_OFFSET)) | ((bc as u64) << H3_BC_OFFSET)
}

fn set_resolution(h: u64, res: i32) -> u64 {
    (h & !(15u64 << H3_RES_OFFSET)) | ((res as u64) << H3_RES_OFFSET)
}

/// `h3NeighborInDirection`: the neighbour of `origin` in direction `dir`, `None` where a
/// pentagon has no neighbour (its deleted k direction).
pub(super) fn neighbor_in_direction(origin: u64, mut dir: i32) -> Option<u64> {
    let mut current = origin;
    let old_base_cell = get_base_cell(current);
    let old_leading = leading_non_zero_digit(current);
    let mut new_rotations = 0;
    let mut r = get_resolution(current) - 1;
    loop {
        if r == -1 {
            let bc = old_base_cell as usize;
            current = set_base_cell(current, BASE_CELL_NEIGHBORS[bc][dir as usize] as i32);
            new_rotations = BASE_CELL_NEIGHBOR_60CCW_ROTS[bc][dir as usize] as i32;
            if get_base_cell(current) == INVALID_BASE_CELL {
                // The deleted k vertex at the base level: this edge borders the ik neighbour.
                current = set_base_cell(current, BASE_CELL_NEIGHBORS[bc][IK_AXES as usize] as i32);
                new_rotations = BASE_CELL_NEIGHBOR_60CCW_ROTS[bc][IK_AXES as usize] as i32;
                current = rotate60ccw(current);
            }
            break;
        }
        let old_digit = get_digit(current, r + 1) as usize;
        let next_dir;
        if (r + 1) & 1 == 1 {
            current = set_digit(current, r + 1, NEW_DIGIT_II[old_digit][dir as usize] as i32);
            next_dir = NEW_ADJUSTMENT_II[old_digit][dir as usize] as i32;
        } else {
            current = set_digit(current, r + 1, NEW_DIGIT_III[old_digit][dir as usize] as i32);
            next_dir = NEW_ADJUSTMENT_III[old_digit][dir as usize] as i32;
        }
        if next_dir != CENTER {
            dir = next_dir;
            r -= 1;
        } else {
            break;
        }
    }
    let new_base_cell = get_base_cell(current);
    if is_base_cell_pentagon(new_base_cell) {
        if leading_non_zero_digit(current) == K_AXES {
            if old_base_cell != new_base_cell {
                if base_cell_is_cw_offset(new_base_cell, BASE_CELL_DATA[old_base_cell as usize].0 as usize) {
                    current = rotate60cw(current);
                } else {
                    current = rotate60ccw(current);
                }
            } else if old_leading == CENTER {
                return None;
            } else if old_leading == JK_AXES {
                current = rotate60ccw(current);
            } else if old_leading == IK_AXES {
                current = rotate60cw(current);
            } else {
                return None;
            }
        }
        for _ in 0..new_rotations {
            current = rotate_pent60ccw(current);
        }
    } else {
        for _ in 0..new_rotations {
            current = rotate60ccw(current);
        }
    }
    Some(current)
}

/// `H3.getLongRes0Cells`: the 122 base cells.
pub fn res0_cells() -> [u64; 122] {
    std::array::from_fn(|bc| set_base_cell((H3_INIT & !(15u64 << H3_MODE_OFFSET)) | (1u64 << H3_MODE_OFFSET), bc as i32))
}

/// `H3.h3ToParent`.
pub fn parent(h: u64) -> u64 {
    let res = get_resolution(h);
    debug_assert!(res > 0, "Input is a base cell");
    set_digit(set_resolution(h, res - 1), res, 7)
}

/// `H3.h3ToChildrenSize(h3)`: 6 for pentagons, else 7.
pub fn children_size(h: u64) -> usize {
    if is_pentagon(h) { 6 } else { 7 }
}

/// `H3.childPosToH3`.
pub fn child(h: u64, pos: usize) -> u64 {
    let res = get_resolution(h) + 1;
    debug_assert!(res <= MAX_RES);
    let c = set_resolution(h, res);
    if pos == 0 {
        return set_digit(c, res, CENTER);
    }
    // Pentagons skip digit 1.
    set_digit(c, res, if is_pentagon(h) { pos as i32 + 1 } else { pos as i32 })
}

const PEN_INTERSECTING_CHILDREN_DIRECTIONS: [i32; 5] = [3, 1, 6, 4, 2];
const HEX_INTERSECTING_CHILDREN_DIRECTIONS: [i32; 6] = [3, 6, 2, 5, 1, 4];

/// `H3.h3ToNotIntersectingChildrenSize`: 5 for pentagons, else 6.
pub fn no_children_size(h: u64) -> usize {
    if is_pentagon(h) { 5 } else { 6 }
}

/// `H3.noChildIntersectingPosToH3`: a finer cell overlapping `h` without being its child
/// (`u64::MAX` where the neighbour doesn't exist, Java's `-1`).
pub fn no_child_intersecting(h: u64, pos: usize) -> u64 {
    let res = get_resolution(h) + 1;
    let c = set_resolution(h, res);
    let (child, dir) = if is_pentagon(h) {
        (set_digit(c, res, pos as i32 + 2), PEN_INTERSECTING_CHILDREN_DIRECTIONS[pos])
    } else {
        (set_digit(c, res, pos as i32 + 1), HEX_INTERSECTING_CHILDREN_DIRECTIONS[pos])
    };
    neighbor_in_direction(child, dir).unwrap_or(u64::MAX)
}

/// `H3.hexRingSize`.
pub fn ring_size(h: u64) -> usize {
    if is_pentagon(h) { 5 } else { 6 }
}

/// `H3.hexRingPosToH3`: pentagons skip ring position 2.
pub fn ring(h: u64, pos: usize) -> u64 {
    let p = if is_pentagon(h) && pos >= 2 { pos + 1 } else { pos };
    neighbor_in_direction(h, DIRECTIONS[p]).unwrap_or(u64::MAX)
}
