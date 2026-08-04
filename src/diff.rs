use crate::model::MergeDirection;
use std::ops::Range;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayLine {
    pub number: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffBlock {
    Equal {
        left: Vec<DisplayLine>,
        right: Vec<DisplayLine>,
    },
    Hunk(DiffHunk),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
    pub id: usize,
    pub left: Vec<DisplayLine>,
    pub right: Vec<DisplayLine>,
    pub left_bytes: Range<usize>,
    pub right_bytes: Range<usize>,
    pub left_start_line: usize,
    pub right_start_line: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WordTone {
    Equal,
    Removed,
    Added,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WordSegment {
    pub text: String,
    pub tone: WordTone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edit {
    Equal,
    Delete,
    Insert,
}

fn myers<T: Eq>(old: &[T], new: &[T]) -> Vec<Edit> {
    let maximum = old.len() + new.len();
    if maximum == 0 {
        return Vec::new();
    }
    let offset = maximum as isize;
    let mut frontier = vec![0_isize; maximum * 2 + 3];
    let index = |diagonal: isize| (diagonal + offset + 1) as usize;
    frontier[index(1)] = 0;
    let mut trace = Vec::new();

    for distance in 0..=maximum {
        let distance = distance as isize;
        for diagonal in (-distance..=distance).step_by(2) {
            let mut x = if diagonal == -distance
                || (diagonal != distance
                    && frontier[index(diagonal - 1)] < frontier[index(diagonal + 1)])
            {
                frontier[index(diagonal + 1)]
            } else {
                frontier[index(diagonal - 1)] + 1
            };
            let mut y = x - diagonal;
            while x < old.len() as isize
                && y < new.len() as isize
                && old[x as usize] == new[y as usize]
            {
                x += 1;
                y += 1;
            }
            frontier[index(diagonal)] = x;
            if x == old.len() as isize && y == new.len() as isize {
                trace.push(frontier.clone());
                return backtrack(old.len(), new.len(), &trace, offset);
            }
        }
        trace.push(frontier.clone());
    }
    unreachable!("Myers search always reaches the end")
}

fn backtrack(old_len: usize, new_len: usize, trace: &[Vec<isize>], offset: isize) -> Vec<Edit> {
    let index = |diagonal: isize| (diagonal + offset + 1) as usize;
    let mut x = old_len as isize;
    let mut y = new_len as isize;
    let mut edits = Vec::new();

    for distance in (1..trace.len()).rev() {
        let previous = &trace[distance - 1];
        let distance = distance as isize;
        let diagonal = x - y;
        let previous_diagonal = if diagonal == -distance
            || (diagonal != distance
                && previous[index(diagonal - 1)] < previous[index(diagonal + 1)])
        {
            diagonal + 1
        } else {
            diagonal - 1
        };
        let previous_x = previous[index(previous_diagonal)];
        let previous_y = previous_x - previous_diagonal;
        while x > previous_x && y > previous_y {
            edits.push(Edit::Equal);
            x -= 1;
            y -= 1;
        }
        if x == previous_x {
            edits.push(Edit::Insert);
            y -= 1;
        } else {
            edits.push(Edit::Delete);
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        edits.push(Edit::Equal);
        x -= 1;
        y -= 1;
    }
    while x > 0 {
        edits.push(Edit::Delete);
        x -= 1;
    }
    while y > 0 {
        edits.push(Edit::Insert);
        y -= 1;
    }
    edits.reverse();
    edits
}

fn lines_with_endings(text: &str) -> Vec<&str> {
    if text.is_empty() {
        Vec::new()
    } else {
        text.split_inclusive('\n').collect()
    }
}

fn offsets(lines: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len() + 1);
    offsets.push(0);
    for line in lines {
        offsets.push(offsets.last().copied().unwrap_or_default() + line.len());
    }
    offsets
}

fn display(lines: &[&str], range: Range<usize>) -> Vec<DisplayLine> {
    range
        .map(|index| DisplayLine {
            number: index + 1,
            text: lines[index].trim_end_matches(['\n', '\r']).to_owned(),
        })
        .collect()
}

pub fn create_diff_blocks(left_text: &str, right_text: &str) -> Vec<DiffBlock> {
    let left_lines = lines_with_endings(left_text);
    let right_lines = lines_with_endings(right_text);
    let left_offsets = offsets(&left_lines);
    let right_offsets = offsets(&right_lines);
    let edits = myers(&left_lines, &right_lines);
    let mut blocks = Vec::new();
    let mut edit_index = 0;
    let mut left_index = 0;
    let mut right_index = 0;
    let mut hunk_id = 0;

    while edit_index < edits.len() {
        if edits[edit_index] == Edit::Equal {
            let left_start = left_index;
            let right_start = right_index;
            while edit_index < edits.len() && edits[edit_index] == Edit::Equal {
                edit_index += 1;
                left_index += 1;
                right_index += 1;
            }
            blocks.push(DiffBlock::Equal {
                left: display(&left_lines, left_start..left_index),
                right: display(&right_lines, right_start..right_index),
            });
            continue;
        }

        let left_start = left_index;
        let right_start = right_index;
        while edit_index < edits.len() && edits[edit_index] != Edit::Equal {
            match edits[edit_index] {
                Edit::Delete => left_index += 1,
                Edit::Insert => right_index += 1,
                Edit::Equal => unreachable!(),
            }
            edit_index += 1;
        }
        blocks.push(DiffBlock::Hunk(DiffHunk {
            id: hunk_id,
            left: display(&left_lines, left_start..left_index),
            right: display(&right_lines, right_start..right_index),
            left_bytes: left_offsets[left_start]..left_offsets[left_index],
            right_bytes: right_offsets[right_start]..right_offsets[right_index],
            left_start_line: left_start + 1,
            right_start_line: right_start + 1,
        }));
        hunk_id += 1;
    }
    blocks
}

pub fn apply_hunk(
    left_text: &str,
    right_text: &str,
    hunk: &DiffHunk,
    direction: MergeDirection,
) -> String {
    match direction {
        MergeDirection::LeftToRight => {
            let replacement = &left_text[hunk.left_bytes.clone()];
            format!(
                "{}{}{}",
                &right_text[..hunk.right_bytes.start],
                replacement,
                &right_text[hunk.right_bytes.end..]
            )
        }
        MergeDirection::RightToLeft => {
            let replacement = &right_text[hunk.right_bytes.clone()];
            format!(
                "{}{}{}",
                &left_text[..hunk.left_bytes.start],
                replacement,
                &left_text[hunk.left_bytes.end..]
            )
        }
    }
}

pub fn word_diff(left: &str, right: &str) -> (Vec<WordSegment>, Vec<WordSegment>) {
    let left_words: Vec<&str> = left.split_inclusive(char::is_whitespace).collect();
    let right_words: Vec<&str> = right.split_inclusive(char::is_whitespace).collect();
    let mut left_segments = Vec::new();
    let mut right_segments = Vec::new();
    let mut left_index = 0;
    let mut right_index = 0;
    for edit in myers(&left_words, &right_words) {
        match edit {
            Edit::Equal => {
                let text = left_words[left_index].to_owned();
                left_segments.push(WordSegment {
                    text: text.clone(),
                    tone: WordTone::Equal,
                });
                right_segments.push(WordSegment {
                    text,
                    tone: WordTone::Equal,
                });
                left_index += 1;
                right_index += 1;
            }
            Edit::Delete => {
                left_segments.push(WordSegment {
                    text: left_words[left_index].to_owned(),
                    tone: WordTone::Removed,
                });
                left_index += 1;
            }
            Edit::Insert => {
                right_segments.push(WordSegment {
                    text: right_words[right_index].to_owned(),
                    tone: WordTone::Added,
                });
                right_index += 1;
            }
        }
    }
    (left_segments, right_segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_hunk(left: &str, right: &str) -> DiffHunk {
        create_diff_blocks(left, right)
            .into_iter()
            .find_map(|block| match block {
                DiffBlock::Hunk(hunk) => Some(hunk),
                DiffBlock::Equal { .. } => None,
            })
            .unwrap()
    }

    #[test]
    fn applies_replacements_in_both_directions() {
        let left = "alpha\nleft value\nomega\n";
        let right = "alpha\nright value\nomega\n";
        let hunk = first_hunk(left, right);
        assert_eq!(
            apply_hunk(left, right, &hunk, MergeDirection::LeftToRight),
            left
        );
        assert_eq!(
            apply_hunk(left, right, &hunk, MergeDirection::RightToLeft),
            right
        );
    }

    #[test]
    fn applies_insertions_without_changing_final_newline_state() {
        let left = "one\ntwo\n";
        let right = "one\ninserted\ntwo\n";
        let hunk = first_hunk(left, right);
        assert_eq!(
            apply_hunk(left, right, &hunk, MergeDirection::LeftToRight),
            left
        );
        assert_eq!(
            apply_hunk(left, right, &hunk, MergeDirection::RightToLeft),
            right
        );
    }

    #[test]
    fn supports_an_empty_file_on_either_side() {
        let left = "created\n";
        let right = "";
        let hunk = first_hunk(left, right);
        assert_eq!(
            apply_hunk(left, right, &hunk, MergeDirection::LeftToRight),
            left
        );
        assert_eq!(
            apply_hunk(left, right, &hunk, MergeDirection::RightToLeft),
            right
        );
    }

    #[test]
    fn finds_two_separate_hunks() {
        let blocks = create_diff_blocks("a\nb\nc\nd\n", "x\nb\nc\ny\n");
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block, DiffBlock::Hunk(_)))
                .count(),
            2
        );
    }

    #[test]
    fn myers_reconstructs_small_sequences() {
        fn sequences() -> Vec<Vec<u8>> {
            let mut output = Vec::new();
            for length in 0..=5 {
                for mask in 0..(1_usize << length) {
                    output.push(
                        (0..length)
                            .map(|index| b'a' + ((mask >> index) & 1) as u8)
                            .collect(),
                    );
                }
            }
            output
        }

        let sequences = sequences();
        for old in &sequences {
            for new in &sequences {
                let edits = myers(old, new);
                let mut old_index = 0;
                let mut new_index = 0;
                for edit in edits {
                    match edit {
                        Edit::Equal => {
                            assert_eq!(old[old_index], new[new_index]);
                            old_index += 1;
                            new_index += 1;
                        }
                        Edit::Delete => old_index += 1,
                        Edit::Insert => new_index += 1,
                    }
                }
                assert_eq!(old_index, old.len());
                assert_eq!(new_index, new.len());
            }
        }
    }
}
