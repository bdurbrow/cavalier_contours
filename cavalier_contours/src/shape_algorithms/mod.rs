use std::collections::{BTreeMap, BTreeSet};

use static_aabb2d_index::{StaticAABB2DIndex, StaticAABB2DIndexBuilder};

use crate::{
    core::{
        math::{dist_squared, Vector2},
        traits::Real,
    },
    polyline::{
        internal::pline_offset::point_valid_for_offset, seg_midpoint, FindIntersectsOptions,
        PlineBasicIntersect, PlineOffsetOptions, PlineOrientation, PlineSource, PlineSourceMut,
        PlineViewData, Polyline,
    },
};

pub struct OffsetLoop<T: Real> {
    pub parent_loop_idx: usize,
    pub indexed_pline: IndexedPolyline<T>,
}

pub struct ClosedPlineSet<T> {
    pub ccw_loops: Vec<Polyline<T>>,
    pub cw_loops: Vec<Polyline<T>>,
}

#[derive(Debug, Clone)]
pub struct IndexedPolyline<T: Real> {
    pub polyline: Polyline<T>,
    pub spatial_index: StaticAABB2DIndex<T>,
}

impl<T> IndexedPolyline<T>
where
    T: Real,
{
    fn new(polyline: Polyline<T>) -> Option<Self> {
        let spatial_index = polyline.create_approx_aabb_index()?;
        Some(Self {
            polyline,
            spatial_index,
        })
    }

    fn parallel_offset(&self, offset: T) -> Vec<Polyline<T>> {
        let opts = PlineOffsetOptions {
            aabb_index: Some(&self.spatial_index),
            handle_self_intersects: false,
            ..Default::default()
        };

        self.polyline.parallel_offset_opt(offset, &opts)
    }
}

pub trait ShapeSource {
    type Num: Real;
    type OutputPolyline;
    type Loop: PlineSource<Num = Self::Num, OutputPolyline = Self::OutputPolyline>;
    fn ccw_loop_count(&self) -> usize;
    fn cw_loop_count(&self) -> usize;
    fn get_loop(&self, i: usize) -> &Self::Loop;
}

pub trait ShapeIndex {
    type Num: Real;
    fn get_loop_index(&self, i: usize) -> Option<&StaticAABB2DIndex<Self::Num>>;
}

impl<T> ShapeIndex for Vec<StaticAABB2DIndex<T>>
where
    T: Real,
{
    type Num = T;
    fn get_loop_index(&self, i: usize) -> Option<&StaticAABB2DIndex<Self::Num>> {
        self.get(i)
    }
}

#[derive(Debug, Clone)]
pub struct Shape<T: Real> {
    pub ccw_plines: Vec<IndexedPolyline<T>>,
    pub cw_plines: Vec<IndexedPolyline<T>>,
    pub plines_index: StaticAABB2DIndex<T>,
}

impl<T> Shape<T>
where
    T: Real,
{
    fn get_loop<'a>(
        i: usize,
        s1: &'a [OffsetLoop<T>],
        s2: &'a [OffsetLoop<T>],
    ) -> &'a OffsetLoop<T> {
        if i < s1.len() {
            &s1[i]
        } else {
            &s2[i]
        }
    }

    pub fn parallel_offset(&self, offset: T) -> Option<Self> {
        // TODO: make part of options parameter.
        let pos_equal_eps = T::from(1e-5).unwrap();
        let offset_tol = T::from(1e-4).unwrap();
        let slice_join_eps = T::from(1e-4).unwrap();
        // generate offset loops
        let mut ccw_offset_loops = Vec::new();
        let mut cw_offset_loops = Vec::new();
        let mut parent_idx = 0;
        for pline in self.ccw_plines.iter() {
            for offset_pline in pline.parallel_offset(offset) {
                // must check if orientation inverted (due to collapse of very narrow or small input)
                if offset_pline.area() < T::zero() {
                    continue;
                }

                let ccw_offset_loop = OffsetLoop {
                    parent_loop_idx: parent_idx,
                    indexed_pline: IndexedPolyline::new(offset_pline)
                        .expect("failed to offset shape polyline"),
                };
                ccw_offset_loops.push(ccw_offset_loop);
            }

            parent_idx += 1;
        }

        for pline in self.cw_plines.iter() {
            for offset_pline in pline.parallel_offset(offset) {
                let area = offset_pline.area();
                let offset_loop = OffsetLoop {
                    parent_loop_idx: parent_idx,
                    indexed_pline: IndexedPolyline::new(offset_pline)
                        .expect("failed to offset shape polyline"),
                };

                if area < T::zero() {
                    cw_offset_loops.push(offset_loop);
                } else {
                    ccw_offset_loops.push(offset_loop);
                }
            }
            parent_idx += 1;
        }

        let offset_loop_count = ccw_offset_loops.len() + cw_offset_loops.len();
        if offset_loop_count == 0 {
            // no offsets remaining
            return None;
        }

        // build spatial index of offset loop approximate bounding boxes
        let offset_loops_index = {
            let mut b =
                StaticAABB2DIndexBuilder::new(ccw_offset_loops.len() + cw_offset_loops.len());
            for index in ccw_offset_loops
                .iter()
                .map(|p| &p.indexed_pline.spatial_index)
            {
                b.add(index.min_x(), index.min_y(), index.max_x(), index.max_y());
            }

            for index in cw_offset_loops
                .iter()
                .map(|p| &p.indexed_pline.spatial_index)
            {
                b.add(index.min_x(), index.min_y(), index.max_x(), index.max_y());
            }

            b.build()
                .expect("failed to build spatial index of offset loop bounds")
        };

        let mut ccw_plines_result = Vec::new();
        let mut cw_plines_result = Vec::new();

        // find all intersects between all offsets to create slice points
        let mut slice_point_sets = Vec::new();
        let mut slice_points_lookup = BTreeMap::<usize, Vec<usize>>::new();
        let mut visited_loop_pairs = BTreeSet::<(usize, usize)>::new();
        let mut query_stack = Vec::new();

        for i in 0..offset_loop_count {
            let loop1 = Self::get_loop(i, &ccw_offset_loops, &cw_offset_loops);
            let spatial_idx1 = &loop1.indexed_pline.spatial_index;
            let query_results = offset_loops_index.query_with_stack(
                spatial_idx1.min_x(),
                spatial_idx1.min_y(),
                spatial_idx1.max_x(),
                spatial_idx1.max_y(),
                &mut query_stack,
            );

            for j in query_results {
                if i == j {
                    // skip same index (no self intersects among the offset loops)
                    continue;
                }

                if visited_loop_pairs.contains(&(j, i)) {
                    // skip reversed index order (would end up comparing the same loops in another
                    // iteration)
                    continue;
                }

                visited_loop_pairs.insert((i, j));

                let loop2 = Self::get_loop(j, &ccw_offset_loops, &cw_offset_loops);

                let intrs_opts = FindIntersectsOptions {
                    pline1_aabb_index: Some(spatial_idx1),
                    // TODO: Use option parameter - pline offset needs to be updated as well?
                    ..Default::default()
                };

                let intersects = loop1
                    .indexed_pline
                    .polyline
                    .find_intersects_opt(&loop2.indexed_pline.polyline, &intrs_opts);

                if intersects.basic_intersects.is_empty()
                    && intersects.overlapping_intersects.is_empty()
                {
                    continue;
                }

                let mut slice_points = Vec::new();

                for intr in intersects.basic_intersects {
                    slice_points.push(intr);
                }

                // add overlapping start and end points
                for overlap_intr in intersects.overlapping_intersects {
                    let start_index1 = overlap_intr.start_index1;
                    let start_index2 = overlap_intr.start_index2;
                    slice_points.push(PlineBasicIntersect {
                        start_index1,
                        start_index2,
                        point: overlap_intr.point1,
                    });
                    slice_points.push(PlineBasicIntersect {
                        start_index1,
                        start_index2,
                        point: overlap_intr.point2,
                    });
                }

                let slice_point_set = SlicePointSet {
                    loop_idx1: i,
                    loop_idx2: j,
                    slice_points,
                };

                slice_points_lookup
                    .entry(i)
                    .or_default()
                    .push(slice_point_sets.len());

                slice_points_lookup
                    .entry(j)
                    .or_default()
                    .push(slice_point_sets.len());

                slice_point_sets.push(slice_point_set);
            }
        }

        // create slices from slice points
        let mut sorted_intrs = Vec::new();
        let mut slices_data = Vec::new();

        let create_slice = |pt1: &DissectionPoint<T>,
                            pt2: &DissectionPoint<T>,
                            offset_loop: &Polyline<T>|
         -> Option<PlineViewData<T>> {
            let v_data = PlineViewData::from_slice_points(
                offset_loop,
                pt1.pos,
                pt1.seg_idx,
                pt2.pos,
                pt2.seg_idx,
                pos_equal_eps,
            );
            v_data
        };

        let is_slice_valid = |v_data: &PlineViewData<T>,
                              offset_loop: &Polyline<T>,
                              parent_idx: usize,
                              query_stack: &mut Vec<usize>|
         -> bool {
            let slice_view = v_data.view(offset_loop);
            let midpoint = seg_midpoint(slice_view.at(0), slice_view.at(1));
            // loop through input polylines and check if slice is too close (skipping parent
            // polyline since it's never too close)
            for input_loop_idx in
                (0..(self.ccw_plines.len() + self.cw_plines.len())).filter(|i| *i != parent_idx)
            {
                let parent_loop = if input_loop_idx < self.ccw_plines.len() {
                    &self.ccw_plines[input_loop_idx]
                } else {
                    &self.cw_plines[input_loop_idx - self.ccw_plines.len()]
                };

                if !point_valid_for_offset(
                    &parent_loop.polyline,
                    offset,
                    &parent_loop.spatial_index,
                    midpoint,
                    query_stack,
                    pos_equal_eps,
                    offset_tol,
                ) {
                    return false;
                }
            }

            true
        };

        for loop_idx in 0..offset_loop_count {
            sorted_intrs.clear();
            let curr_loop = Self::get_loop(loop_idx, &ccw_offset_loops, &cw_offset_loops);

            if let Some(slice_point_set_idxs) = slice_points_lookup.get(&loop_idx) {
                // gather all the intersects for the current loop
                sorted_intrs.extend(slice_point_set_idxs.iter().flat_map(|set_idx| {
                    let set = &slice_point_sets[*set_idx];
                    debug_assert!(set.loop_idx1 == loop_idx || set.loop_idx2 == loop_idx);
                    let loop_is_first_index = set.loop_idx1 == loop_idx;
                    set.slice_points.iter().map(move |intr_pt| {
                        let seg_idx = if loop_is_first_index {
                            intr_pt.start_index1
                        } else {
                            intr_pt.start_index2
                        };
                        DissectionPoint {
                            seg_idx,
                            pos: intr_pt.point,
                        }
                    })
                }));

                // sort the intersect points along direction of polyline
                sorted_intrs.sort_unstable_by(|a, b| {
                    // sort by the segment index, then if both intersects on the same segment sort
                    // by distance from start of segment
                    a.seg_idx.cmp(&b.seg_idx).then_with(|| {
                        let seg_start = curr_loop.indexed_pline.polyline.at(a.seg_idx).pos();
                        let dist1 = dist_squared(a.pos, seg_start);
                        let dist2 = dist_squared(b.pos, seg_start);
                        dist1.partial_cmp(&dist2).unwrap()
                    })
                });

                // construct valid slices to later be stitched together
                if sorted_intrs.len() == 1 {
                    // treat whole loop as slice
                    let v_data =
                        PlineViewData::from_entire_pline(&curr_loop.indexed_pline.polyline);
                    if is_slice_valid(
                        &v_data,
                        &curr_loop.indexed_pline.polyline,
                        curr_loop.parent_loop_idx,
                        &mut query_stack,
                    ) {
                        slices_data.push(DissectedSlice {
                            source_idx: loop_idx,
                            v_data,
                        });
                    }
                } else {
                    // create slices from adjacent points
                    let mut windows = sorted_intrs.windows(2);
                    while let Some([pt1, pt2]) = windows.next() {
                        let v_data = create_slice(pt1, pt2, &curr_loop.indexed_pline.polyline);
                        if let Some(v_data) = v_data {
                            if is_slice_valid(
                                &v_data,
                                &curr_loop.indexed_pline.polyline,
                                curr_loop.parent_loop_idx,
                                &mut query_stack,
                            ) {
                                slices_data.push(DissectedSlice {
                                    source_idx: loop_idx,
                                    v_data,
                                });
                            }
                        }

                        // collect slice from last to start
                        let pt1 = sorted_intrs.last().unwrap();
                        let pt2 = &sorted_intrs[0];
                        let v_data = create_slice(pt1, pt2, &curr_loop.indexed_pline.polyline);
                        if let Some(v_data) = v_data {
                            if is_slice_valid(
                                &v_data,
                                &curr_loop.indexed_pline.polyline,
                                curr_loop.parent_loop_idx,
                                &mut query_stack,
                            ) {
                                slices_data.push(DissectedSlice {
                                    source_idx: loop_idx,
                                    v_data,
                                });
                            }
                        }
                    }
                }
            } else {
                // no intersects but still must test distance of one vertex position since it may be
                // inside another offset (completely eclipsed by island offset)
                let v_data = PlineViewData::from_entire_pline(&curr_loop.indexed_pline.polyline);
                if is_slice_valid(
                    &v_data,
                    &curr_loop.indexed_pline.polyline,
                    curr_loop.parent_loop_idx,
                    &mut query_stack,
                ) {
                    // TODO: for now just cloning polylines to result to avoid complexity
                    if curr_loop.indexed_pline.polyline.orientation()
                        == PlineOrientation::CounterClockwise
                    {
                        ccw_plines_result.push(curr_loop.indexed_pline.clone());
                    } else {
                        cw_plines_result.push(curr_loop.indexed_pline.clone())
                    }
                }
            }
        }

        // stitch slices together
        let slice_starts_aabb_index = {
            let mut builder = StaticAABB2DIndexBuilder::new(slices_data.len());
            for slice in slices_data.iter() {
                let start_point = slice.v_data.updated_start.pos();
                builder.add(
                    start_point.x - slice_join_eps,
                    start_point.y - slice_join_eps,
                    start_point.x + slice_join_eps,
                    start_point.y + slice_join_eps,
                );
            }
            builder.build().unwrap()
        };
        let mut visited_slices_idxs = vec![false; slices_data.len()];
        let mut query_results = Vec::new();
        for slice_idx in 0..slices_data.len() {
            if visited_slices_idxs[slice_idx] {
                continue;
            }
            visited_slices_idxs[slice_idx] = true;

            let mut current_index = slice_idx;
            let mut loop_count = 0;
            let max_loop_count = slices_data.len();
            let mut current_pline = Polyline::new();
            loop {
                if loop_count > max_loop_count {
                    // prevent infinite loop
                    unreachable!(
                        "loop_count exceeded max_loop_count while stitching slices together"
                    );
                }
                loop_count += 1;

                let curr_slice = &slices_data[current_index];
                let source_loop =
                    Self::get_loop(curr_slice.source_idx, &ccw_offset_loops, &cw_offset_loops);
                let slice_view = curr_slice.v_data.view(&source_loop.indexed_pline.polyline);
                current_pline.extend_remove_repeat(&slice_view, pos_equal_eps);

                query_results.clear();
                let slice_end_point = curr_slice.v_data.end_point;
                let mut aabb_index_visitor = |i: usize| {
                    if !visited_slices_idxs[i] {
                        query_results.push(i);
                    }
                };
                slice_starts_aabb_index.visit_query_with_stack(
                    slice_end_point.x - slice_join_eps,
                    slice_end_point.y - slice_join_eps,
                    slice_end_point.x + slice_join_eps,
                    slice_end_point.y + slice_join_eps,
                    &mut aabb_index_visitor,
                    &mut query_stack,
                );

                if query_results.is_empty() {
                    if current_pline.vertex_count() > 1 {
                        current_pline.remove_last();
                        current_pline.set_is_closed(true);
                    }
                    let is_ccw = current_pline.orientation() == PlineOrientation::CounterClockwise;
                    if is_ccw {
                        ccw_plines_result.push(IndexedPolyline::new(current_pline).unwrap());
                    } else {
                        cw_plines_result.push(IndexedPolyline::new(current_pline).unwrap());
                    }
                    break;
                }

                current_index = query_results
                    .iter()
                    .find_map(|i| {
                        let slice = &slices_data[*i];
                        if slice.source_idx == curr_slice.source_idx {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| query_results[0]);

                current_pline.remove_last();
                visited_slices_idxs[current_index] = true;
            }
        }

        let plines_index = {
            let mut b =
                StaticAABB2DIndexBuilder::new(ccw_plines_result.len() + cw_plines_result.len());
            for pl in ccw_plines_result.iter() {
                b.add(
                    pl.spatial_index.min_x(),
                    pl.spatial_index.min_y(),
                    pl.spatial_index.max_x(),
                    pl.spatial_index.max_y(),
                );
            }
            for pl in cw_plines_result.iter() {
                b.add(
                    pl.spatial_index.min_x(),
                    pl.spatial_index.min_y(),
                    pl.spatial_index.max_x(),
                    pl.spatial_index.max_y(),
                );
            }
            b.build().unwrap()
        };

        Some(Shape {
            ccw_plines: ccw_plines_result,
            cw_plines: cw_plines_result,
            plines_index,
        })
    }
}

// fn stitch_slices_into_closed_polylines<

// intersects between two offset loops
#[derive(Debug, Clone)]
struct SlicePointSet<T> {
    loop_idx1: usize,
    loop_idx2: usize,
    slice_points: Vec<PlineBasicIntersect<T>>,
}

#[derive(Debug, Clone, Copy)]
struct DissectionPoint<T> {
    seg_idx: usize,
    pos: Vector2<T>,
}

#[derive(Debug, Clone, Copy)]
struct DissectedSlice<T> {
    source_idx: usize,
    v_data: PlineViewData<T>,
}

// fn create_offset_loops<T>(input_set: &ClosedPlineSet<T>, abs_offset: T)
// where
//     T: Real,
// {
//     let mut result = ClosedPlineSet {
//         ccw_loops: Vec::new(),
//         cw_loops: Vec::new(),
//     };

//     let mut parent_idx = 0;
//     for pline in input_set.ccw_loops.iter() {
//         for offset_pline in pline.parallel_offset(abs_offset) {
//             // must check if orientation inverted (due to collapse of very narrow or small input)
//             if offset_pline.area() < T::zero() {
//                 continue;
//             }

//             let spatial_index = offset_pline.create_approx_aabb_index();
//         }
//     }

//     result
// }
