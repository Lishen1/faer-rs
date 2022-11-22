use dyn_stack::{DynStack, SizeOverflow, StackReq};
use faer_core::{
    permutation::{permute_rows, PermutationIndicesRef},
    solve::*,
    temp_mat_req, temp_mat_uninit, ComplexField, Conj, MatMut, MatRef, Parallelism,
};
use reborrow::*;

fn solve_impl<T: ComplexField>(
    lu_factors: MatRef<'_, T>,
    conj_lhs: Conj,
    row_perm: PermutationIndicesRef<'_>,
    col_perm: PermutationIndicesRef<'_>,
    dst: MatMut<'_, T>,
    rhs: Option<MatRef<'_, T>>,
    conj_rhs: Conj,
    parallelism: Parallelism,
    stack: DynStack<'_>,
) {
    // LU = P(row_fwd) × A × P(col_inv)

    // P(row_inv) ConjA?(LU) P(col_fwd) X = ConjB?(B)
    // X = P(col_inv) ConjA?(U)^-1 ConjA?(L)^-1 P(row_fwd) ConjB?(B)

    let n = lu_factors.ncols();
    let k = dst.ncols();

    temp_mat_uninit! {
        let (mut temp, _) = unsafe { temp_mat_uninit::<T>(n, k, stack) };
    }

    // temp <- P(row_fwd) B
    let src = match rhs {
        Some(rhs) => rhs,
        None => dst.rb(),
    };
    permute_rows(temp.rb_mut(), src, row_perm);

    // temp <- ConjA?(L)^-1 P(row_fwd) ConjB?(B)
    solve_unit_lower_triangular_in_place(
        lu_factors,
        conj_lhs,
        temp.rb_mut(),
        conj_rhs,
        parallelism,
    );

    // temp <- ConjA?(U)^-1 ConjA?(L)^-1 P(row_fwd) B
    solve_upper_triangular_in_place(lu_factors, conj_lhs, temp.rb_mut(), Conj::No, parallelism);

    permute_rows(dst, temp.rb(), col_perm.inverse());
}

pub fn solve_req<T: 'static>(
    lu_nrows: usize,
    lu_ncols: usize,
    rhs_ncols: usize,
    parallelism: Parallelism,
) -> Result<StackReq, SizeOverflow> {
    let _ = lu_ncols;
    let _ = parallelism;
    temp_mat_req::<T>(lu_nrows, rhs_ncols)
}

pub fn solve_to<T: ComplexField>(
    dst: MatMut<'_, T>,
    lu_factors: MatRef<'_, T>,
    conj_lhs: Conj,
    row_perm: PermutationIndicesRef<'_>,
    col_perm: PermutationIndicesRef<'_>,
    rhs: MatRef<'_, T>,
    conj_rhs: Conj,
    parallelism: Parallelism,
    stack: DynStack<'_>,
) {
    solve_impl(
        lu_factors,
        conj_lhs,
        row_perm,
        col_perm,
        dst,
        Some(rhs),
        conj_rhs,
        parallelism,
        stack,
    )
}

pub fn solve_in_place<T: ComplexField>(
    lu_factors: MatRef<'_, T>,
    conj_lhs: Conj,
    row_perm: PermutationIndicesRef<'_>,
    col_perm: PermutationIndicesRef<'_>,
    rhs: MatMut<'_, T>,
    conj_rhs: Conj,
    parallelism: Parallelism,
    stack: DynStack<'_>,
) {
    solve_impl(
        lu_factors,
        conj_lhs,
        row_perm,
        col_perm,
        rhs,
        None,
        conj_rhs,
        parallelism,
        stack,
    );
}

#[cfg(test)]
mod tests {
    use assert2::assert as fancy_assert;
    use dyn_stack::GlobalMemBuffer;
    use faer_core::{c64, mul::matmul, Mat};
    use rand::random;

    use crate::full_pivoting::compute::{lu_in_place, lu_in_place_req};

    use super::*;

    macro_rules! make_stack {
        ($req: expr) => {
            DynStack::new(&mut GlobalMemBuffer::new($req))
        };
    }

    fn test_solve_to<T: ComplexField>(mut gen: impl FnMut() -> T, epsilon: T::Real) {
        (0..32).chain((1..16).map(|i| i * 32)).for_each(|n| {
            for conj_lhs in [Conj::No, Conj::Yes] {
                for conj_rhs in [Conj::No, Conj::Yes] {
                    let a = Mat::with_dims(|_, _| gen(), n, n);
                    let mut lu = a.clone();
                    let a = a.as_ref();
                    let mut lu = lu.as_mut();

                    let k = 32;
                    let rhs = Mat::with_dims(|_, _| gen(), n, k);
                    let rhs = rhs.as_ref();
                    let mut sol = Mat::<T>::zeros(n, k);
                    let mut sol = sol.as_mut();

                    let mut row_perm = vec![0_usize; n];
                    let mut row_perm_inv = vec![0_usize; n];
                    let mut col_perm = vec![0_usize; n];
                    let mut col_perm_inv = vec![0_usize; n];

                    let parallelism = Parallelism::Rayon(0);

                    let (_, row_perm, col_perm) = lu_in_place(
                        lu.rb_mut(),
                        &mut row_perm,
                        &mut row_perm_inv,
                        &mut col_perm,
                        &mut col_perm_inv,
                        parallelism,
                        make_stack!(lu_in_place_req::<T>(n, n, parallelism).unwrap()),
                    );

                    solve_to(
                        sol.rb_mut(),
                        lu.rb(),
                        conj_lhs,
                        row_perm.rb(),
                        col_perm.rb(),
                        rhs,
                        conj_rhs,
                        parallelism,
                        make_stack!(solve_req::<T>(n, n, k, parallelism).unwrap()),
                    );

                    let mut rhs_reconstructed = Mat::zeros(n, k);
                    let mut rhs_reconstructed = rhs_reconstructed.as_mut();

                    matmul(
                        rhs_reconstructed.rb_mut(),
                        Conj::No,
                        a,
                        conj_lhs,
                        sol.rb(),
                        Conj::No,
                        None,
                        T::one(),
                        parallelism,
                    );

                    for j in 0..k {
                        for i in 0..n {
                            let target = match conj_rhs {
                                Conj::No => rhs[(i, j)],
                                Conj::Yes => rhs[(i, j)].conj(),
                            };

                            fancy_assert!((rhs_reconstructed[(i, j)] - target).abs() < epsilon)
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn test_solve_to_f64() {
        test_solve_to(|| random::<f64>(), 1e-10_f64)
    }

    #[test]
    fn test_solve_to_c64() {
        test_solve_to(|| c64::new(random(), random()), 1e-10_f64)
    }
}
