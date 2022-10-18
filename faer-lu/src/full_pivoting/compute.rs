use core::ops::{Add, Mul, Neg};
use std::mem::{size_of, transmute_copy};

use assert2::{assert as fancy_assert, debug_assert as fancy_debug_assert};
use bytemuck::cast;
use dyn_stack::DynStack;
use faer_core::mul::matmul;
use faer_core::permutation::PermutationIndicesMut;
use faer_core::{ColRef, MatMut, MatRef, RowRef};
use num_traits::{Inv, One, Signed, Zero};
use pulp::Simd;
use reborrow::*;

#[inline(always)]
fn best_f64<S: pulp::Simd>(
    simd: S,
    best_value: S::f64s,
    best_indices: S::u64s,
    data: S::f64s,
    indices: S::u64s,
) -> (S::f64s, S::u64s) {
    let value = simd.f64s_abs(data);
    let is_better = simd.f64s_greater_than(value, best_value);
    (
        simd.m64s_select_f64s(is_better, value, best_value),
        simd.m64s_select_u64s(is_better, indices, best_indices),
    )
}

#[inline(always)]
fn best_in_col_f64_generic<S: pulp::Simd>(
    simd: S,
    iota: S::u64s,
    data: &[f64],
) -> (S::f64s, S::u64s, f64, u64) {
    let (head, tail) = S::f64s_as_simd(data);

    let lane_count = core::mem::size_of::<S::u64s>() / core::mem::size_of::<u64>();
    let increment1 = simd.u64s_splat(lane_count as u64);
    let increment3 = simd.u64s_splat(3 * lane_count as u64);

    let mut best_value0 = simd.f64s_splat(0.0);
    let mut best_value1 = simd.f64s_splat(0.0);
    let mut best_value2 = simd.f64s_splat(0.0);
    let mut best_indices0 = simd.u64s_splat(0);
    let mut best_indices1 = simd.u64s_splat(0);
    let mut best_indices2 = simd.u64s_splat(0);
    let mut indices0 = iota;
    let mut indices1 = simd.u64s_add(indices0, increment1);
    let mut indices2 = simd.u64s_add(indices1, increment1);

    let head_chunks = head.chunks_exact(3);

    for data in head_chunks.clone() {
        let d0 = data[0];
        let d1 = data[1];
        let d2 = data[2];
        (best_value0, best_indices0) = best_f64(simd, best_value0, best_indices0, d0, indices0);
        (best_value1, best_indices1) = best_f64(simd, best_value1, best_indices1, d1, indices1);
        (best_value2, best_indices2) = best_f64(simd, best_value2, best_indices2, d2, indices2);
        indices0 = simd.u64s_add(indices0, increment3);
        indices1 = simd.u64s_add(indices1, increment3);
        indices2 = simd.u64s_add(indices2, increment3);
    }

    (best_value0, best_indices0) =
        best_f64(simd, best_value0, best_indices0, best_value1, best_indices1);
    (best_value0, best_indices0) =
        best_f64(simd, best_value0, best_indices0, best_value2, best_indices2);

    for data in head_chunks.remainder().iter().copied() {
        (best_value0, best_indices0) = best_f64(simd, best_value0, best_indices0, data, indices0);
        indices0 = simd.u64s_add(indices0, increment1);
    }

    let mut best_value_scalar = 0.0;
    let mut best_index_scalar = 0;
    let mut index = (head.len() * lane_count) as u64;
    for data in tail.iter().copied() {
        (best_value_scalar, best_index_scalar) = best_f64(
            pulp::Scalar::new(),
            best_value_scalar,
            best_index_scalar,
            data,
            index,
        );
        index += 1;
    }

    (
        best_value0,
        best_indices0,
        best_value_scalar,
        best_index_scalar,
    )
}

#[inline(always)]
fn update_and_best_in_col_f64_generic<S: pulp::Simd>(
    simd: S,
    iota: S::u64s,
    dst: &mut [f64],
    lhs: &[f64],
    rhs: f64,
) -> (S::f64s, S::u64s, f64, u64) {
    let lane_count = core::mem::size_of::<S::u64s>() / core::mem::size_of::<u64>();
    let len = dst.len();

    let offset = dst.as_ptr().align_offset(size_of::<S::f64s>());
    let ((dst_prefix, dst_suffix), (lhs_prefix, lhs_suffix)) = (
        dst.split_at_mut(offset.min(len)),
        lhs.split_at(offset.min(len)),
    );

    let mut best_value_scalar = 0.0;
    let mut best_index_scalar = 0;
    let mut index = 0_u64;

    for (dst, lhs) in dst_prefix.iter_mut().zip(lhs_prefix) {
        let new_dst = f64::mul_add(*lhs, rhs, *dst);
        *dst = new_dst;
        (best_value_scalar, best_index_scalar) = best_f64(
            pulp::Scalar::new(),
            best_value_scalar,
            best_index_scalar,
            new_dst,
            index,
        );
        index += 1;
    }

    let (dst_head, dst_tail) = S::f64s_as_mut_simd(dst_suffix);
    let (lhs_head, lhs_tail) = S::f64s_as_simd(lhs_suffix);

    let increment1 = simd.u64s_splat(1 * lane_count as u64);
    let increment2 = simd.u64s_splat(2 * lane_count as u64);

    let mut best_value0 = simd.f64s_splat(0.0);
    let mut best_value1 = simd.f64s_splat(0.0);
    let mut best_indices0 = simd.u64s_splat(0);
    let mut best_indices1 = simd.u64s_splat(0);
    let mut indices0 = simd.u64s_add(iota, simd.u64s_splat(offset as u64));
    let mut indices1 = simd.u64s_add(indices0, increment1);

    let mut dst_head_chunks = dst_head.chunks_exact_mut(2);
    let lhs_head_chunks = lhs_head.chunks_exact(2);

    let rhs_v = simd.f64s_splat(rhs);
    for (dst, lhs) in (&mut dst_head_chunks).zip(lhs_head_chunks.clone()) {
        let (dst0, dst1) = dst.split_at_mut(1);
        let dst0 = &mut dst0[0];
        let dst1 = &mut dst1[0];
        let lhs0 = lhs[0];
        let lhs1 = lhs[1];

        let new_dst0 = simd.f64s_mul_adde(lhs0, rhs_v, *dst0);
        let new_dst1 = simd.f64s_mul_adde(lhs1, rhs_v, *dst1);
        *dst0 = new_dst0;
        *dst1 = new_dst1;

        (best_value0, best_indices0) =
            best_f64(simd, best_value0, best_indices0, new_dst0, indices0);
        (best_value1, best_indices1) =
            best_f64(simd, best_value1, best_indices1, new_dst1, indices1);
        indices0 = simd.u64s_add(indices0, increment2);
        indices1 = simd.u64s_add(indices1, increment2);
    }

    (best_value0, best_indices0) =
        best_f64(simd, best_value0, best_indices0, best_value1, best_indices1);

    for (dst, lhs) in dst_head_chunks
        .into_remainder()
        .iter_mut()
        .zip(lhs_head_chunks.remainder().iter().copied())
    {
        let new_dst = simd.f64s_mul_adde(lhs, rhs_v, *dst);
        *dst = new_dst;
        (best_value0, best_indices0) =
            best_f64(simd, best_value0, best_indices0, new_dst, indices0);
    }

    index = (offset + dst_head.len() * lane_count) as u64;
    for (dst, lhs) in dst_tail.iter_mut().zip(lhs_tail) {
        let new_dst = f64::mul_add(*lhs, rhs, *dst);
        *dst = new_dst;
        (best_value_scalar, best_index_scalar) = best_f64(
            pulp::Scalar::new(),
            best_value_scalar,
            best_index_scalar,
            new_dst,
            index,
        );
        index += 1;
    }

    (
        best_value0,
        best_indices0,
        best_value_scalar,
        best_index_scalar,
    )
}

#[inline(always)]
fn best_in_col_f64x2<S: Simd>(
    simd: S,
    data: &[f64],
    reduce: impl Fn(f64, u64, &[f64], &[u64]) -> (f64, u64),
) -> (f64, u64) {
    let (best_value, best_indices, best_value_s, best_index_s) =
        best_in_col_f64_generic(simd, cast([0, 1_u64]), data);
    let best_value_v: [f64; 2] = cast(best_value);
    let best_index_v: [u64; 2] = cast(best_indices);
    reduce(best_value_s, best_index_s, &best_value_v, &best_index_v)
}
#[inline(always)]
fn best_in_col_f64x4<S: Simd>(
    simd: S,
    data: &[f64],
    reduce: impl Fn(f64, u64, &[f64], &[u64]) -> (f64, u64),
) -> (f64, u64) {
    let (best_value, best_indices, best_value_s, best_index_s) =
        best_in_col_f64_generic(simd, cast([0, 1, 2, 3_u64]), data);
    let best_value_v: [f64; 4] = cast(best_value);
    let best_index_v: [u64; 4] = cast(best_indices);
    reduce(best_value_s, best_index_s, &best_value_v, &best_index_v)
}
#[inline(always)]
fn best_in_col_f64x8<S: Simd>(
    simd: S,
    data: &[f64],
    reduce: impl Fn(f64, u64, &[f64], &[u64]) -> (f64, u64),
) -> (f64, u64) {
    let (best_value, best_indices, best_value_s, best_index_s) =
        best_in_col_f64_generic(simd, cast([0, 1, 2, 3, 4, 5, 6, 7_u64]), data);
    let best_value_v: [f64; 8] = cast(best_value);
    let best_index_v: [u64; 8] = cast(best_indices);
    reduce(best_value_s, best_index_s, &best_value_v, &best_index_v)
}

#[inline(always)]
fn update_and_best_in_col_f64x2<S: Simd>(
    simd: S,
    dst: &mut [f64],
    lhs: &[f64],
    rhs: f64,
    reduce: impl Fn(f64, u64, &[f64], &[u64]) -> (f64, u64),
) -> (f64, u64) {
    let (best_value, best_indices, best_value_s, best_index_s) =
        update_and_best_in_col_f64_generic(simd, cast([0, 1_u64]), dst, lhs, rhs);
    let best_value_v: [f64; 2] = cast(best_value);
    let best_index_v: [u64; 2] = cast(best_indices);
    reduce(best_value_s, best_index_s, &best_value_v, &best_index_v)
}
#[inline(always)]
fn update_and_best_in_col_f64x4<S: Simd>(
    simd: S,
    dst: &mut [f64],
    lhs: &[f64],
    rhs: f64,
    reduce: impl Fn(f64, u64, &[f64], &[u64]) -> (f64, u64),
) -> (f64, u64) {
    let (best_value, best_indices, best_value_s, best_index_s) =
        update_and_best_in_col_f64_generic(simd, cast([0, 1, 2, 3_u64]), dst, lhs, rhs);
    let best_value_v: [f64; 4] = cast(best_value);
    let best_index_v: [u64; 4] = cast(best_indices);
    reduce(best_value_s, best_index_s, &best_value_v, &best_index_v)
}
#[inline(always)]
fn update_and_best_in_col_f64x8<S: Simd>(
    simd: S,
    dst: &mut [f64],
    lhs: &[f64],
    rhs: f64,
    reduce: impl Fn(f64, u64, &[f64], &[u64]) -> (f64, u64),
) -> (f64, u64) {
    let (best_value, best_indices, best_value_s, best_index_s) =
        update_and_best_in_col_f64_generic(simd, cast([0, 1, 2, 3, 4, 5, 6, 7_u64]), dst, lhs, rhs);
    let best_value_v: [f64; 8] = cast(best_value);
    let best_index_v: [u64; 8] = cast(best_indices);
    reduce(best_value_s, best_index_s, &best_value_v, &best_index_v)
}

#[inline(always)]
fn best_in_col_f64<S: Simd>(simd: S, data: &[f64]) -> (f64, u64) {
    let lane_count = core::mem::size_of::<S::u64s>() / core::mem::size_of::<u64>();
    let reduce =
        |mut best_value_scalar, mut best_index_scalar, best_value: &[f64], best_indices: &[u64]| {
            for (data, index) in best_value.iter().copied().zip(best_indices.iter().copied()) {
                (best_value_scalar, best_index_scalar) = best_f64(
                    pulp::Scalar::new(),
                    best_value_scalar,
                    best_index_scalar,
                    data,
                    index,
                );
            }
            (best_value_scalar, best_index_scalar)
        };
    if lane_count == 8 {
        best_in_col_f64x8(simd, data, reduce)
    } else if lane_count == 4 {
        best_in_col_f64x4(simd, data, reduce)
    } else if lane_count == 2 {
        best_in_col_f64x2(simd, data, reduce)
    } else {
        let (best_values, best_indices, _, _) = best_in_col_f64_generic(simd, cast(0_u64), data);
        (cast(best_values), cast(best_indices))
    }
}

#[inline(always)]
fn update_and_best_in_col_f64<S: Simd>(
    simd: S,
    dst: &mut [f64],
    lhs: &[f64],
    rhs: f64,
) -> (f64, u64) {
    let lane_count = core::mem::size_of::<S::u64s>() / core::mem::size_of::<u64>();
    let reduce =
        |mut best_value_scalar, mut best_index_scalar, best_value: &[f64], best_indices: &[u64]| {
            for (data, index) in best_value.iter().copied().zip(best_indices.iter().copied()) {
                (best_value_scalar, best_index_scalar) = best_f64(
                    pulp::Scalar::new(),
                    best_value_scalar,
                    best_index_scalar,
                    data,
                    index,
                );
            }
            (best_value_scalar, best_index_scalar)
        };
    if lane_count == 8 {
        update_and_best_in_col_f64x8(simd, dst, lhs, rhs, reduce)
    } else if lane_count == 4 {
        update_and_best_in_col_f64x4(simd, dst, lhs, rhs, reduce)
    } else if lane_count == 2 {
        update_and_best_in_col_f64x2(simd, dst, lhs, rhs, reduce)
    } else {
        let (best_values, best_indices, _, _) =
            update_and_best_in_col_f64_generic(simd, cast(0_u64), dst, lhs, rhs);
        (cast(best_values), cast(best_indices))
    }
}

#[inline(always)]
fn best_in_matrix_f64(matrix: MatRef<'_, f64>) -> (usize, usize) {
    struct BestInMat<'a>(MatRef<'a, f64>);
    impl<'a> pulp::WithSimd for BestInMat<'a> {
        type Output = (usize, usize);

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let matrix = self.0;
            fancy_debug_assert!(matrix.row_stride() == 1);

            let m = matrix.nrows();
            let n = matrix.ncols();
            let mut best_row = 0;
            let mut best_col = 0;
            let mut best_value = 0.0;

            for j in 0..n {
                unsafe {
                    let ptr = matrix.col_unchecked(j).as_ptr();
                    let col = core::slice::from_raw_parts(ptr, m);
                    let (best_value_in_col, best_index_in_col) = best_in_col_f64(simd, col);
                    if best_value_in_col > best_value {
                        best_value = best_value_in_col;
                        best_row = best_index_in_col as usize;
                        best_col = j;
                    }
                }
            }

            (best_row, best_col)
        }
    }

    pulp::Arch::new().dispatch(BestInMat(matrix))
}

#[inline(always)]
fn update_and_best_in_matrix_f64(
    matrix: MatMut<'_, f64>,
    lhs: ColRef<'_, f64>,
    rhs: RowRef<'_, f64>,
) -> (usize, usize) {
    struct UpdateAndBestInMat<'a>(MatMut<'a, f64>, ColRef<'a, f64>, RowRef<'a, f64>);
    impl<'a> pulp::WithSimd for UpdateAndBestInMat<'a> {
        type Output = (usize, usize);

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let UpdateAndBestInMat(mut matrix, lhs, rhs) = self;
            fancy_debug_assert!(matrix.row_stride() == 1);
            fancy_debug_assert!(lhs.row_stride() == 1);

            let m = matrix.nrows();
            let n = matrix.ncols();
            let mut best_row = 0;
            let mut best_col = 0;
            let mut best_value = 0.0;

            unsafe {
                let lhs = core::slice::from_raw_parts(lhs.as_ptr(), m);

                for j in 0..n {
                    let rhs = -*rhs.get_unchecked(j);

                    let ptr = matrix.rb_mut().col_unchecked(j).as_ptr();
                    let dst = core::slice::from_raw_parts_mut(ptr, m);

                    let (best_value_in_col, best_index_in_col) =
                        update_and_best_in_col_f64(simd, dst, lhs, rhs);
                    if best_value_in_col > best_value {
                        best_value = best_value_in_col;
                        best_row = best_index_in_col as usize;
                        best_col = j;
                    }
                }
            }

            (best_row, best_col)
        }
    }

    pulp::Arch::new().dispatch(UpdateAndBestInMat(matrix, lhs, rhs))
}

#[inline]
fn best_in_matrix<T>(matrix: MatRef<'_, T>) -> (usize, usize)
where
    T: Zero + Signed + PartialOrd + 'static,
{
    // let is_f32 = core::any::TypeId::of::<T>() == core::any::TypeId::of::<f32>();
    let is_f64 = core::any::TypeId::of::<T>() == core::any::TypeId::of::<f64>();

    let is_col_major = matrix.row_stride() == 1;
    let is_row_major = matrix.col_stride() == 1;

    if is_col_major && is_f64 {
        best_in_matrix_f64(unsafe { transmute_copy(&matrix) })
    } else if is_row_major && is_f64 {
        best_in_matrix_f64(unsafe { transmute_copy(&matrix.transpose()) })
    } else {
        let m = matrix.nrows();
        let n = matrix.ncols();

        let mut max = T::zero();
        let mut max_row = 0;
        let mut max_col = 0;

        for j in 0..n {
            for i in 0..m {
                let abs = unsafe { matrix.get_unchecked(i, j).abs() };
                if abs > max {
                    max_row = i;
                    max_col = j;
                    max = abs;
                }
            }
        }

        (max_row, max_col)
    }
}

#[inline]
fn rank_one_update_and_best_in_matrix<T>(
    mut dst: MatMut<'_, T>,
    lhs: ColRef<'_, T>,
    rhs: RowRef<'_, T>,
) -> (usize, usize)
where
    T: Zero + One + Clone + Send + Sync + Signed + PartialOrd + 'static,
    for<'a> &'a T: Add<Output = T> + Mul<Output = T> + Neg<Output = T> + Inv<Output = T>,
{
    let is_f64 = core::any::TypeId::of::<T>() == core::any::TypeId::of::<f64>();

    let is_col_major = dst.row_stride() == 1 && lhs.row_stride() == 1;
    let is_row_major = dst.col_stride() == 1 && rhs.col_stride() == 1;

    let dst_row_stride = dst.row_stride();
    let dst_col_stride = dst.col_stride();
    let dst_nrows = dst.nrows();
    let dst_ncols = dst.ncols();

    if is_f64 && (is_col_major || is_row_major) {
        unsafe {
            let dst = MatMut::from_raw_parts(
                dst.as_ptr() as *mut f64,
                dst_nrows,
                dst_ncols,
                dst_row_stride,
                dst_col_stride,
            );
            if is_col_major {
                update_and_best_in_matrix_f64(dst, transmute_copy(&lhs), transmute_copy(&rhs))
            } else if is_row_major {
                update_and_best_in_matrix_f64(
                    dst.transpose(),
                    transmute_copy(&rhs.transpose()),
                    transmute_copy(&lhs.transpose()),
                )
            } else {
                unreachable!()
            }
        }
    } else {
        matmul(
            dst.rb_mut(),
            lhs.as_2d(),
            rhs.as_2d(),
            Some(&T::one()),
            &-&T::one(),
            1,
            DynStack::new(&mut []),
        );
        best_in_matrix(dst.rb())
    }
}

#[inline]
unsafe fn lu_in_place_unblocked<T>(
    mut matrix: MatMut<'_, T>,
    row_transpositions: &mut [usize],
    col_transpositions: &mut [usize],
    _n_threads: usize,
    _stack: DynStack<'_>,
) -> usize
where
    T: Zero + One + Clone + Send + Sync + Signed + PartialOrd + 'static,
    for<'a> &'a T: Add<Output = T> + Mul<Output = T> + Neg<Output = T> + Inv<Output = T>,
{
    let m = matrix.nrows();
    let n = matrix.ncols();

    fancy_debug_assert!(row_transpositions.len() == m);
    fancy_debug_assert!(col_transpositions.len() == n);

    if n == 0 || m == 0 {
        return 0;
    }

    let size = m.min(n);

    let mut n_transpositions = 0;

    let (mut max_row, mut max_col) = best_in_matrix(matrix.rb());
    row_transpositions[0] = max_row;
    col_transpositions[0] = max_col;

    if max_row != 0 {
        n_transpositions += 1;

        let (_, top, _, bot) = matrix.rb_mut().split_at_unchecked(1, 0);
        let row_j = top.row_unchecked(0);
        let row_max = bot.row_unchecked(max_row - 1);
        swap_cols(row_j.transpose(), row_max.transpose());
    }

    if max_col != 0 {
        n_transpositions += 1;

        let (_, _, left, right) = matrix.rb_mut().split_at_unchecked(0, 1);
        let col_j = left.col_unchecked(0);
        let col_max = right.col_unchecked(max_col - 1);
        swap_cols(col_j, col_max);
    }

    for k in 0..size {
        let inv = matrix.rb().get_unchecked(k, k).inv();
        for i in k + 1..m {
            let elem = matrix.rb_mut().get_unchecked(i, k);
            *elem = &*elem * &inv;
        }

        if k + 1 == size {
            break;
        }

        let (_, top_right, bottom_left, bottom_right) =
            matrix.rb_mut().split_at_unchecked(k + 1, k + 1);

        (max_row, max_col) = rank_one_update_and_best_in_matrix(
            bottom_right,
            bottom_left.col(k).rb(),
            top_right.row(k).rb(),
        );
        max_row += k + 1;
        max_col += k + 1;
        row_transpositions[k + 1] = max_row;
        col_transpositions[k + 1] = max_col;

        if max_row != k + 1 {
            n_transpositions += 1;

            let (_, top, _, bot) = matrix.rb_mut().split_at_unchecked(k + 2, 0);
            let row_j = top.row_unchecked(k + 1);
            let row_max = bot.row_unchecked(max_row - k - 2);
            swap_cols(row_j.transpose(), row_max.transpose());
        }

        if max_col != k + 1 {
            n_transpositions += 1;

            let (_, _, left, right) = matrix.rb_mut().split_at_unchecked(0, k + 2);
            let col_j = left.col_unchecked(k + 1);
            let col_max = right.col_unchecked(max_col - k - 2);
            swap_cols(col_j, col_max);
        }
    }

    n_transpositions
}

#[inline]
unsafe fn swap_cols<T>(mut col_j: faer_core::ColMut<T>, mut col_max: faer_core::ColMut<T>) {
    let m = col_j.nrows();
    for k in 0..m {
        core::mem::swap(
            col_j.rb_mut().get_unchecked(k),
            col_max.rb_mut().get_unchecked(k),
        );
    }
}

pub fn lu_in_place<'out, T>(
    matrix: MatMut<'_, T>,
    row_perm: &'out mut [usize],
    row_perm_inv: &'out mut [usize],
    col_perm: &'out mut [usize],
    col_perm_inv: &'out mut [usize],
    n_threads: usize,
    stack: DynStack<'_>,
) -> (
    usize,
    PermutationIndicesMut<'out>,
    PermutationIndicesMut<'out>,
)
where
    T: Zero + One + Clone + Send + Sync + Signed + PartialOrd + 'static,
    for<'a> &'a T: Add<Output = T> + Mul<Output = T> + Neg<Output = T> + Inv<Output = T>,
{
    let m = matrix.nrows();
    let n = matrix.ncols();
    fancy_assert!(row_perm.len() == m);
    fancy_assert!(row_perm_inv.len() == m);
    fancy_assert!(col_perm.len() == n);
    fancy_assert!(col_perm_inv.len() == n);

    let (mut row_transpositions, stack) = stack.make_with(m, |i| i);
    let (mut col_transpositions, stack) = stack.make_with(n, |i| i);

    let n_transpositions = unsafe {
        lu_in_place_unblocked(
            matrix,
            &mut row_transpositions,
            &mut col_transpositions,
            n_threads,
            stack,
        )
    };

    row_perm.iter_mut().enumerate().for_each(|(i, e)| *e = i);
    for (i, t) in row_transpositions.iter().copied().enumerate() {
        row_perm.swap(i, t);
    }

    col_perm.iter_mut().enumerate().for_each(|(i, e)| *e = i);
    for (i, t) in col_transpositions.iter().copied().enumerate() {
        col_perm.swap(i, t);
    }

    for (i, p) in row_perm.iter().copied().enumerate() {
        row_perm_inv[p] = i;
    }
    for (i, p) in col_perm.iter().copied().enumerate() {
        col_perm_inv[p] = i;
    }

    unsafe {
        (
            n_transpositions,
            PermutationIndicesMut::new_unchecked(row_perm, row_perm_inv),
            PermutationIndicesMut::new_unchecked(col_perm, col_perm_inv),
        )
    }
}

#[cfg(test)]
mod tests {
    use assert_approx_eq::assert_approx_eq;
    use dyn_stack::{GlobalMemBuffer, StackReq};
    use faer_core::{mul, Mat};
    use rand::random;

    use super::*;

    fn reconstruct_matrix(lu_factors: MatRef<'_, f64>) -> Mat<f64> {
        let m = lu_factors.nrows();
        let n = lu_factors.ncols();

        let size = n.min(m);

        let mut a_reconstructed = Mat::zeros(m, n);

        let (_, l_top, _, l_bot) = lu_factors.submatrix(0, 0, m, size).split_at(size, 0);
        let (_, _, u_left, u_right) = lu_factors.submatrix(0, 0, size, n).split_at(0, size);

        use mul::triangular::BlockStructure::*;

        let (dst_top_left, dst_top_right, dst_bot_left, dst_bot_right) =
            a_reconstructed.as_mut().split_at(size, size);

        mul::triangular::matmul(
            dst_top_left,
            Rectangular,
            l_top,
            UnitTriangularLower,
            u_left,
            TriangularUpper,
            None,
            &1.0,
            12,
            DynStack::new(&mut dyn_stack::GlobalMemBuffer::new(
                mul::triangular::matmul_req::<f64>(
                    Rectangular,
                    UnitTriangularLower,
                    TriangularUpper,
                    size,
                    size,
                    size,
                    12,
                )
                .unwrap(),
            )),
        );
        mul::triangular::matmul(
            dst_top_right,
            Rectangular,
            l_top,
            UnitTriangularLower,
            u_right,
            Rectangular,
            None,
            &1.0,
            12,
            DynStack::new(&mut dyn_stack::GlobalMemBuffer::new(
                mul::triangular::matmul_req::<f64>(
                    Rectangular,
                    UnitTriangularLower,
                    Rectangular,
                    size,
                    n - size,
                    size,
                    12,
                )
                .unwrap(),
            )),
        );
        mul::triangular::matmul(
            dst_bot_left,
            Rectangular,
            l_bot,
            Rectangular,
            u_left,
            TriangularUpper,
            None,
            &1.0,
            12,
            DynStack::new(&mut dyn_stack::GlobalMemBuffer::new(
                mul::triangular::matmul_req::<f64>(
                    Rectangular,
                    Rectangular,
                    TriangularUpper,
                    m - size,
                    size,
                    size,
                    12,
                )
                .unwrap(),
            )),
        );
        mul::triangular::matmul(
            dst_bot_right,
            Rectangular,
            l_bot,
            Rectangular,
            u_right,
            Rectangular,
            None,
            &1.0,
            12,
            DynStack::new(&mut dyn_stack::GlobalMemBuffer::new(
                mul::triangular::matmul_req::<f64>(
                    Rectangular,
                    Rectangular,
                    Rectangular,
                    m - size,
                    n - size,
                    size,
                    12,
                )
                .unwrap(),
            )),
        );

        a_reconstructed
    }

    #[test]
    fn compute_lu() {
        for (m, n) in [
            (2, 2),
            (4, 2),
            (2, 1),
            (4, 4),
            (20, 20),
            (20, 2),
            (2, 4),
            (2, 20),
            (40, 20),
            (20, 40),
        ] {
            dbg!(m, n);
            let mut mat = Mat::with_dims(|_i, _j| random::<f64>(), m, n);
            let mat_orig = mat.clone();
            let mut row_perm = vec![0; m];
            let mut row_perm_inv = vec![0; m];
            let mut col_perm = vec![0; n];
            let mut col_perm_inv = vec![0; n];

            let mut mem = GlobalMemBuffer::new(StackReq::new::<f64>(1024 * 1024 * 1024));
            let mut stack = DynStack::new(&mut mem);

            lu_in_place(
                mat.as_mut(),
                &mut row_perm,
                &mut row_perm_inv,
                &mut col_perm,
                &mut col_perm_inv,
                1,
                stack.rb_mut(),
            );
            let reconstructed = reconstruct_matrix(mat.as_ref());

            dbg!(&row_perm, &col_perm, &reconstructed, &mat_orig, &mat);
            for i in 0..m {
                for j in 0..n {
                    assert_approx_eq!(mat_orig[(row_perm[i], col_perm[j])], reconstructed[(i, j)]);
                }
            }
        }
    }
}