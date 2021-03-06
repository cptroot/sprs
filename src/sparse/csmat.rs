///! A sparse matrix in the Compressed Sparse Row/Column format
///
/// In the CSR format, a matrix is a structure containing three vectors:
/// indptr, indices, and data
/// These vectors satisfy the relation
/// for i in [0, nrows],
/// A(i, indices[indptr[i]..indptr[i+1]]) = data[indptr[i]..indptr[i+1]]
/// In the CSC format, the relation is
/// A(indices[indptr[i]..indptr[i+1]], i) = data[indptr[i]..indptr[i+1]]

use std::iter::{Enumerate};
use std::default::Default;
use std::slice::{self, Windows};
use std::ops::{Deref, DerefMut, Add, Sub, Mul, Range};
use std::mem;
use num::traits::Num;

use dense_mats::{StorageOrder, Tensor, MatOwned};

use sparse::permutation::PermView;
use sparse::vec::{CsVec, CsVecView};
use sparse::compressed::SpMatView;
use sparse::binop;
use sparse::prod;
use errors::SprsError;


pub type CsMatOwned<N> = CsMat<N, Vec<usize>, Vec<usize>, Vec<N>>;
pub type CsMatView<'a, N> = CsMat<N, &'a [usize], &'a [usize], &'a [N]>;

// FIXME: a fixed size array would be better, but no Deref impl
pub type CsMatVecView<'a, N> = CsMat<N, Vec<usize>, &'a [usize], &'a [N]>;

/// Describe the storage of a CsMat
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompressedStorage {
    /// Compressed row storage
    CSR,
    /// Compressed column storage
    CSC
}

impl CompressedStorage {
    /// Get the other storage, ie return CSC if we were CSR, and vice versa
    pub fn other_storage(&self) -> CompressedStorage {
        match *self {
            CSR => CSC,
            CSC => CSR,
        }
    }
}

pub use self::CompressedStorage::{CSC, CSR};

/// Iterator on the matrix' outer dimension
/// Implemented over an iterator on the indptr array
pub struct OuterIterator<'iter, N: 'iter> {
    inner_len: usize,
    indptr_iter: Enumerate<Windows<'iter, usize>>,
    indices: &'iter [usize],
    data: &'iter [N],
}

/// Iterator on the matrix' outer dimension, permuted
/// Implemented over an iterator on the indptr array
pub struct OuterIteratorPerm<'iter, 'perm: 'iter, N: 'iter> {
    inner_len: usize,
    outer_ind_iter: Range<usize>,
    indptr: &'iter [usize],
    indices: &'iter [usize],
    data: &'iter [N],
    perm: PermView<'perm>,
}


/// Outer iteration on a compressed matrix yields
/// a tuple consisting of the outer index and of a sparse vector
/// containing the associated inner dimension
impl <'iter, N: 'iter + Copy>
Iterator
for OuterIterator<'iter, N> {
    type Item = (usize, CsVec<N, &'iter[usize], &'iter[N]>);
    #[inline]
    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        match self.indptr_iter.next() {
            None => None,
            Some((outer_ind, window)) => {
                let inner_start = window[0];
                let inner_end = window[1];
                let indices = &self.indices[inner_start..inner_end];
                let data = &self.data[inner_start..inner_end];
                // safety derives from the structure checks in the constructors
                unsafe {
                    let vec = CsVec::new_raw(self.inner_len, indices.len(),
                                             indices.as_ptr(), data.as_ptr());
                    Some((outer_ind, vec))
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indptr_iter.size_hint()
    }
}

/// Permuted outer iteration on a compressed matrix yields
/// a tuple consisting of the outer index and of a sparse vector
/// containing the associated inner dimension
impl <'iter, 'perm: 'iter, N: 'iter + Copy>
Iterator
for OuterIteratorPerm<'iter, 'perm, N> {
    type Item = (usize, CsVec<N, &'iter[usize], &'iter[N]>);
    #[inline]
    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        match self.outer_ind_iter.next() {
            None => None,
            Some(outer_ind) => {
                let outer_ind_perm = self.perm.at(outer_ind);
                let inner_start = self.indptr[outer_ind_perm];
                let inner_end = self.indptr[outer_ind_perm + 1];
                let indices = &self.indices[inner_start..inner_end];
                let data = &self.data[inner_start..inner_end];
                // safety derives from the structure checks in the constructors
                unsafe {
                    let vec = CsVec::new_raw(self.inner_len, indices.len(),
                                             indices.as_ptr(), data.as_ptr());
                    Some((outer_ind_perm, vec))
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.outer_ind_iter.size_hint()
    }
}

/// Reverse outer iteration on a compressed matrix yields
/// a tuple consisting of the outer index and of a sparse vector
/// containing the associated inner dimension
///
/// Only the outer dimension iteration is reverted. If you wish to also
/// revert the inner dimension, you should call rev() again when iterating
/// the vector.
impl <'iter, N: 'iter + Copy>
DoubleEndedIterator
for OuterIterator<'iter, N> {
    #[inline]
    fn next_back(&mut self) -> Option<<Self as Iterator>::Item> {
        match self.indptr_iter.next_back() {
            None => None,
            Some((outer_ind, window)) => {
                let inner_start = window[0];
                let inner_end = window[1];
                let indices = &self.indices[inner_start..inner_end];
                let data = &self.data[inner_start..inner_end];
                // safety derives from the structure checks in the constructors
                unsafe {
                    let vec = CsVec::new_raw(self.inner_len, indices.len(),
                                             indices.as_ptr(), data.as_ptr());
                    Some((outer_ind, vec))
                }
            }
        }
    }
}

impl <'iter, N: 'iter + Copy> ExactSizeIterator for OuterIterator<'iter, N> {
    fn len(&self) -> usize {
        self.indptr_iter.len()
    }
}

/// Compressed matrix in the CSR or CSC format.
#[derive(PartialEq, Debug)]
pub struct CsMat<N, IptrStorage, IndStorage, DataStorage>
where IptrStorage: Deref<Target=[usize]>,
      IndStorage: Deref<Target=[usize]>,
      DataStorage: Deref<Target=[N]> {
    storage: CompressedStorage,
    nrows : usize,
    ncols : usize,
    nnz : usize,
    indptr : IptrStorage,
    indices : IndStorage,
    data : DataStorage
}

impl<'a, N:'a + Copy> CsMat<N, Vec<usize>, &'a [usize], &'a [N]> {
    /// Create a borrowed row or column CsMat matrix from raw data,
    /// without checking their validity
    ///
    /// This is unsafe because algorithms are free to assume
    /// that properties guaranteed by check_compressed_structure are enforced.
    /// For instance, non out-of-bounds indices can be relied upon to
    /// perform unchecked slice access.
    pub unsafe fn new_vecview_raw(
        storage: CompressedStorage, nrows : usize, ncols: usize,
        indptr : Vec<usize>, indices : *const usize, data : *const N
        )
    -> CsMatVecView<'a, N> {
        let nnz = indptr[1];
        CsMat {
            storage: storage,
            nrows : nrows,
            ncols: ncols,
            nnz : nnz,
            indptr : indptr,
            indices : slice::from_raw_parts(indices, nnz),
            data : slice::from_raw_parts(data, nnz),
        }
    }
}

impl<'a, N:'a + Copy> CsMat<N, &'a [usize], &'a [usize], &'a [N]> {
    /// Create a borrowed CsMat matrix from sliced data,
    /// checking their validity
    pub fn new_borrowed(
        storage: CompressedStorage, nrows : usize, ncols: usize,
        indptr : &'a[usize], indices : &'a[usize], data : &'a[N]
        )
    -> Result<CsMatView<'a, N>, SprsError> {
        let m = CsMat {
            storage: storage,
            nrows : nrows,
            ncols: ncols,
            nnz : data.len(),
            indptr : indptr,
            indices : indices,
            data : data,
        };
        m.check_compressed_structure().and(Ok(m))
    }

    /// Create a borrowed CsMat matrix from raw data,
    /// without checking their validity
    ///
    /// This is unsafe because algorithms are free to assume
    /// that properties guaranteed by check_compressed_structure are enforced.
    /// For instance, non out-of-bounds indices can be relied upon to
    /// perform unchecked slice access.
    pub unsafe fn new_raw(
        storage: CompressedStorage, nrows : usize, ncols: usize,
        indptr : *const usize, indices : *const usize, data : *const N
        )
    -> CsMatView<'a, N> {
        let outer = match storage {
            CSR => nrows,
            CSC => ncols,
        };
        let indptr = slice::from_raw_parts(indptr, outer + 1);
        let nnz = *indptr.get_unchecked(outer);
        CsMat {
            storage: storage,
            nrows : nrows,
            ncols: ncols,
            nnz : nnz,
            indptr : indptr,
            indices : slice::from_raw_parts(indices, nnz),
            data : slice::from_raw_parts(data, nnz),
        }
    }

    /// Get a view into count contiguous outer dimensions, starting from i.
    /// 
    /// eg this gets the rows from i to i + count in a CSR matrix
    pub fn middle_outer_views(&self,
                              i: usize, count: usize
                             ) -> Result<CsMatView<'a, N>, SprsError> {
        // TODO: check for potential overflow?
        if count == 0 {
            return Err(SprsError::EmptyBlock);
        }
        let iend = i + count;
        if i >= self.outer_dims() || iend > self.outer_dims() {
            return Err(SprsError::OutOfBoundsIndex);
        }
        Ok(CsMat {
            storage: self.storage,
            nrows: count,
            ncols: self.cols(),
            nnz: self.indptr[iend] - self.indptr[i],
            indptr: &self.indptr[i..(iend+1)],
            indices: &self.indices[..],
            data: &self.data[..],
        })
    }

}

impl<N: Copy> CsMat<N, Vec<usize>, Vec<usize>, Vec<N>> {
    /// Create an empty CsMat for building purposes
    pub fn empty(storage: CompressedStorage, inner_size: usize
                ) -> CsMatOwned<N> {
        let (nrows, ncols) = match storage {
            CSR => (0, inner_size),
            CSC => (inner_size, 0)
        };
        CsMat {
            storage: storage,
            nrows: nrows,
            ncols: ncols,
            nnz: 0,
            indptr: vec![0; 1],
            indices: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Create a new CsMat representing the zero matrix.
    /// Hence it has no non-zero elements.
    pub fn zero(rows: usize, cols: usize) -> CsMatOwned<N> {
        CsMat {
            storage: CSR,
            nrows: rows,
            ncols: cols,
            nnz: 0,
            indptr: vec![0; rows + 1],
            indices: Vec::new(),
            data: Vec::new(),
        }
    }

    /// Reserve the storage for the given additional number of nonzero data
    pub fn reserve_outer_dim(&mut self, outer_dim_additional: usize) {
        self.indptr.reserve(outer_dim_additional);
    }

    /// Reserve the storage for the given additional number of nonzero data
    pub fn reserve_nnz(&mut self, nnz_additional: usize) {
        self.indices.reserve(nnz_additional);
        self.data.reserve(nnz_additional);
    }

    /// Reserve the storage for the given number of nonzero data
    pub fn reserve_outer_dim_exact(&mut self, outer_dim_lim: usize) {
        self.indptr.reserve_exact(outer_dim_lim + 1);
    }

    /// Reserve the storage for the given number of nonzero data
    pub fn reserve_nnz_exact(&mut self, nnz_lim: usize) {
        self.indices.reserve_exact(nnz_lim);
        self.data.reserve_exact(nnz_lim);
    }

    /// Create an owned CsMat matrix from moved data,
    /// checking their validity
    pub fn new_owned(
        storage: CompressedStorage, nrows : usize, ncols: usize,
        indptr : Vec<usize>, indices : Vec<usize>, data : Vec<N>
        )
    -> Result<CsMatOwned<N>, SprsError> {
        let m = CsMat {
            storage: storage,
            nrows : nrows,
            ncols: ncols,
            nnz : data.len(),
            indptr : indptr,
            indices : indices,
            data : data,
        };
        m.check_compressed_structure().and(Ok(m))
    }

    /// Append an outer dim to an existing matrix, compressing it in the process
    pub fn append_outer(mut self, data: &[N]) -> Self where N: Num {
        for (inner_ind, val) in data.iter().enumerate() {
            if *val != N::zero() {
                self.indices.push(inner_ind);
                self.data.push(*val);
                self.nnz += 1;
            }
        }
        match self.storage {
            CSR => self.nrows += 1,
            CSC => self.ncols += 1
        }
        self.indptr.push(self.nnz);
        self
    }

    /// Append an outer dim to an existing matrix, provided by a sparse vector
    pub fn append_outer_csvec(mut self, vec: CsVec<N,&[usize],&[N]>) -> Self {
        assert_eq!(self.inner_dims(), vec.dim());
        for (ind, val) in vec.indices().iter().zip(vec.data()) {
            self.indices.push(*ind);
            self.data.push(*val);
        }
        match self.storage {
            CSR => self.nrows += 1,
            CSC => self.ncols += 1
        }
        self.nnz += vec.nnz();
        self.indptr.push(self.nnz);
        self
    }
}

impl<N: Num + Copy> CsMat<N, Vec<usize>, Vec<usize>, Vec<N>> {
    /// Identity matrix
    ///
    /// ```rust
    /// use sprs::{CsMat, CsVec};
    /// let eye = CsMat::eye(sprs::CSR, 5);
    /// let x = CsVec::new_owned(5, vec![0, 2, 4], vec![1., 2., 3.]).unwrap();
    /// let y = &eye * &x;
    /// assert_eq!(x, y);
    /// ```
    pub fn eye(storage: CompressedStorage, dim: usize
              ) -> CsMatOwned<N> {
        let n = dim;
        let indptr = (0..n+1).collect();
        let indices = (0..n).collect();
        let data = vec![N::one(); n];
        CsMat {
            storage: storage,
            nrows: n,
            ncols: n,
            nnz: n,
            indptr: indptr,
            indices: indices,
            data: data,
        }
    }

}

impl<N, IptrStorage, IndStorage, DataStorage>
CsMat<N, IptrStorage, IndStorage, DataStorage>
where N: Copy,
      IptrStorage: Deref<Target=[usize]>,
      IndStorage: Deref<Target=[usize]>,
      DataStorage: Deref<Target=[N]> {

    /// Return an outer iterator for the matrix
    ///
    /// This can be used for iterating over the rows (resp. cols) of
    /// a CSR (resp. CSC) matrix.
    ///
    /// ```rust
    /// use sprs::{CsMat};
    /// let eye = CsMat::eye(sprs::CSR, 5);
    /// for (row_ind, row_vec) in eye.outer_iterator() {
    ///     let (col_ind, val): (_, f64) = row_vec.iter().next().unwrap();
    ///     assert_eq!(row_ind, col_ind);
    ///     assert_eq!(val, 1.);
    /// }
    /// ```
    pub fn outer_iterator<'a>(&'a self) -> OuterIterator<'a, N> {
        let inner_len = match self.storage {
            CSR => self.ncols,
            CSC => self.nrows
        };
        OuterIterator {
            inner_len: inner_len,
            indptr_iter: self.indptr.windows(2).enumerate(),
            indices: &self.indices[..],
            data: &self.data[..],
        }
    }

    /// Return an outer iterator over P*A, as well as the proper permutation
    /// for iterating over the inner dimension of P*A*P^T
    /// Unstable
    pub fn outer_iterator_perm<'a, 'perm: 'a>(
        &'a self, perm: PermView<'perm>)
    -> OuterIteratorPerm<'a, 'perm, N> {
        let (inner_len, oriented_perm) = match self.storage {
            CSR => (self.ncols, perm.reborrow()),
            CSC => (self.nrows, perm.reborrow_inv())
        };
        let n = self.indptr.len() - 1;
        OuterIteratorPerm {
            inner_len: inner_len,
            outer_ind_iter: (0..n),
            indptr: &self.indptr[..],
            indices: &self.indices[..],
            data: &self.data[..],
            perm: oriented_perm
        }
    }

    /// The underlying storage of this matrix
    pub fn storage(&self) -> CompressedStorage {
        self.storage
    }

    /// The number of rows of this matrix
    pub fn rows(&self) -> usize {
        self.nrows
    }

    /// The number of cols of this matrix
    pub fn cols(&self) -> usize {
        self.ncols
    }

    /// The number of non-zero elements this matrix stores.
    /// This is often relevant for the complexity of most sparse matrix
    /// algorithms, which are often linear in the number of non-zeros.
    pub fn nb_nonzero(&self) -> usize {
        self.nnz
    }

    /// Number of outer dimensions, that ie equal to self.rows() for a CSR
    /// matrix, and equal to self.cols() for a CSC matrix
    pub fn outer_dims(&self) -> usize {
        match self.storage {
            CSR => self.nrows,
            CSC => self.ncols
        }
    }

    /// Number of inner dimensions, that ie equal to self.cols() for a CSR
    /// matrix, and equal to self.rows() for a CSC matrix
    pub fn inner_dims(&self) -> usize {
        match self.storage {
            CSC => self.nrows,
            CSR => self.ncols
        }
    }

    /// Access the element located at row i and column j.
    /// Will return None if there is no non-zero element at this location.
    ///
    /// This access is logarithmic in the number of non-zeros
    /// in the corresponding outer slice. It is therefore advisable not to rely
    /// on this for algorithms, and prefer outer_iterator() which accesses
    /// elements in storage order.
    pub fn at(&self, &(i,j) : &(usize, usize)) -> Option<N> {
        assert!(i < self.nrows);
        assert!(j < self.ncols);

        match self.storage {
            CSR => self.at_outer_inner(&(i,j)),
            CSC => self.at_outer_inner(&(j,i))
        }
    }

    /// Get a view into the i-th outer dimension (eg i-th row for a CSR matrix)
    pub fn outer_view(&self, i: usize) -> Option<CsVecView<N>> {
        if i >= self.outer_dims() {
            return None;
        }
        let start = self.indptr[i];
        let stop = self.indptr[i+1];
        // safety derives from the structure checks in the constructors
        unsafe {
            Some(CsVec::new_raw(self.inner_dims(),
                                self.indices[start..stop].len(),
                                self.indices[start..stop].as_ptr(),
                                self.data[start..stop].as_ptr()))
        }
    }

    /// Iteration on outer blocks of size block_size
    pub fn outer_block_iter(&self, block_size: usize
                           ) -> ChunkOuterBlocks<N> {
        let m = CsMatView {
            storage: self.storage,
            nrows: self.rows(),
            ncols: self.cols(),
            nnz: self.nnz,
            indptr: &self.indptr[..],
            indices: &self.indices[..],
            data: &self.data[..],
        };
        ChunkOuterBlocks {
            mat: m,
            dims_in_bloc: block_size,
            bloc_count: 0,
        }
    }

    /// The array of offsets in the indices() and data() slices.
    /// The elements of the slice at outer dimension i
    /// are available between the elements indptr[i] and indptr[i+1]
    /// in the indices() and data() slices.
    ///
    /// # Example
    ///
    /// ```rust
    /// use sprs::{CsMat, CsMatOwned};
    /// let eye : CsMatOwned<f64> = CsMat::eye(sprs::CSR, 5);
    /// // get the element of row 3
    /// // there is only one element in this row, with a column index of 3
    /// // and a value of 1.
    /// let loc = eye.indptr()[3];
    /// assert_eq!(eye.indptr()[4], loc + 1);
    /// assert_eq!(loc, 3);
    /// assert_eq!(eye.indices()[loc], 3);
    /// assert_eq!(eye.data()[loc], 1.);
    /// ```
    pub fn indptr(&self) -> &[usize] {
        &self.indptr[..]
    }

    /// The inner dimension location for each non-zero value. See
    /// the documentation of indptr() for more explanations.
    pub fn indices(&self) -> &[usize] {
        &self.indices[..]
    }

    /// The non-zero values. See the documentation of indptr()
    /// for more explanations.
    pub fn data(&self) -> &[N] {
        &self.data[..]
    }

    /// Test whether the matrix is in CSC storage
    pub fn is_csc(&self) -> bool {
        self.storage == CSC
    }

    /// Test whether the matrix is in CSR storage
    pub fn is_csr(&self) -> bool {
        self.storage == CSR
    }

    /// Transpose a matrix in place
    /// No allocation required (this is simply a storage order change)
    pub fn transpose_mut(&mut self) {
        mem::swap(&mut self.nrows, &mut self.ncols);
        self.storage = self.storage.other_storage();
    }

    /// Transpose a matrix in place
    /// No allocation required (this is simply a storage order change)
    pub fn transpose_into(mut self) -> Self {
        self.transpose_mut();
        self
    }

    /// Transposed view of this matrix
    /// No allocation required (this is simply a storage order change)
    pub fn transpose_view(&self) -> CsMatView<N> {
        CsMatView {
            storage: self.storage.other_storage(),
            nrows: self.ncols,
            ncols: self.nrows,
            nnz: self.nnz,
            indptr: &self.indptr[..],
            indices: &self.indices[..],
            data: &self.data[..],
        }
    }

    /// Get an owned version of this matrix. If the matrix was already
    /// owned, this will make a deep copy.
    pub fn to_owned(&self) -> CsMatOwned<N> {
        CsMatOwned {
            storage: self.storage,
            nrows: self.nrows,
            ncols: self.ncols,
            nnz: self.nnz,
            indptr: self.indptr.to_vec(),
            indices: self.indices.to_vec(),
            data: self.data.to_vec(),
        }
    }

    /// Access an element given its outer_ind and inner_ind.
    /// Will return None if there is no non-zero element at this location.
    ///
    /// This access is logarithmic in the number of non-zeros
    /// in the corresponding outer slice. It is therefore advisable not to rely
    /// on this for algorithms, and prefer outer_iterator() which accesses
    /// elements in storage order.
    pub fn at_outer_inner(&self,
                          &(outer_ind, inner_ind): &(usize, usize)
                         ) -> Option<N> {
        self.outer_view(outer_ind).and_then(|vec| vec.at(inner_ind))
    }

    /// Check the structure of CsMat components
    /// This will ensure that:
    /// * indptr is of length outer_dim() + 1
    /// * indices and data have the same length, nnz == indptr[outer_dims()]
    /// * indptr is sorted
    /// * indptr values do not exceed usize::MAX / 2, as that would mean
    ///   indices and indptr would take more space than the addressable memory
    /// * indices is sorted for each outer slice
    /// * indices are lower than inner_dims()
    pub fn check_compressed_structure(&self) -> Result<(), SprsError> {
        let outer = self.outer_dims();

        if self.indptr.len() != outer + 1 {
            return Err(SprsError::BadIndptrLength);
        }
        if self.indices.len() != self.data.len() {
            return Err(SprsError::DataIndicesMismatch);
        }
        let nnz = self.indices.len();
        if nnz != self.nnz {
            return Err(SprsError::BadNnzCount);
        }
        if let Some(&max_indptr) = self.indptr.iter().max() {
            if max_indptr > nnz {
                return Err(SprsError::OutOfBoundsIndptr);
            }
            if max_indptr > usize::max_value() / 2 {
                return Err(SprsError::OutOfBoundsIndptr);
            }
        }
        else {
            unreachable!();
        }

        if ! self.indptr.deref().windows(2).all(|x| x[0] <= x[1]) {
            return Err(SprsError::UnsortedIndptr);
        }

        // check that the indices are sorted for each row
        for (_, vec) in self.outer_iterator() {
            try!(vec.check_structure());
        }

        Ok(())
    }

    /// Return a view into the current matrix
    pub fn borrowed(&self) -> CsMatView<N> {
        CsMat {
            storage: self.storage,
            nrows: self.nrows,
            ncols: self.ncols,
            nnz: self.nnz,
            indptr: &self.indptr[..],
            indices: &self.indices[..],
            data: &self.data[..],
        }
    }
}

impl<N, IptrStorage, IndStorage, DataStorage>
CsMat<N, IptrStorage, IndStorage, DataStorage>
where N: Copy + Default,
      IptrStorage: Deref<Target=[usize]>,
      IndStorage: Deref<Target=[usize]>,
      DataStorage: Deref<Target=[N]> {

    /// Create a matrix mathematically equal to this one, but with the
    /// opposed storage (a CSC matrix will be converted to CSR, and vice versa)
    pub fn to_other_storage(&self) -> CsMatOwned<N> {
        let mut indptr = vec![0; self.inner_dims() + 1];
        let mut indices = vec![0; self.nb_nonzero()];
        let mut data = vec![N::default(); self.nb_nonzero()];
        let borrowed = self.borrowed();
        raw::convert_mat_storage(borrowed,
                                 &mut indptr, &mut indices, &mut data);
        CsMat::new_owned(self.storage().other_storage(),
                         self.rows(), self.cols(),
                         indptr, indices, data).unwrap()
    }

    /// Create a new CSC matrix equivalent to this one.
    /// A new matrix will be created even if this matrix was already CSC.
    pub fn to_csc(&self) -> CsMatOwned<N> {
        match self.storage {
            CSR => self.to_other_storage(),
            CSC => self.to_owned()
        }
    }

    /// Create a new CSR matrix equivalent to this one.
    /// A new matrix will be created even if this matrix was already CSR.
    pub fn to_csr(&self) -> CsMatOwned<N> {
        match self.storage {
            CSR => self.to_owned(),
            CSC => self.to_other_storage()
        }
    }

}

impl<N, IptrStorage, IndStorage, DataStorage>
CsMat<N, IptrStorage, IndStorage, DataStorage>
where
N: Copy,
IptrStorage: DerefMut<Target=[usize]>,
IndStorage: DerefMut<Target=[usize]>,
DataStorage: DerefMut<Target=[N]> {

    /// Mutable access to the non zero values
    pub fn data_mut(&mut self) -> &mut [N] {
        &mut self.data[..]
    }

    /// Sparse matrix self-multiplication by a scalar
    pub fn scale(&mut self, val: N) where N: Num + Copy {
        for data in self.data_mut() {
            *data = *data * val;
        }
    }

}

mod raw {
    use super::{CsMatView};
    use std::mem::swap;

    /// Copy-convert a CsMat into the oppposite storage.
    /// Can be used to implement CSC <-> CSR conversions, or to implement
    /// same-storage (copy) transposition.
    ///
    /// # Panics
    /// Panics if indptr contains non-zero values
    ///
    /// Panics if the output slices don't match the input matrices'
    /// corresponding slices.
    pub fn convert_mat_storage<N: Copy>(mat: CsMatView<N>,
                                        indptr: &mut [usize],
                                        indices: &mut[usize],
                                        data: &mut [N]) {
        assert_eq!(indptr.len(), mat.inner_dims() + 1);
        assert_eq!(indices.len(), mat.indices().len());
        assert_eq!(data.len(), mat.data().len());

        assert!(indptr.iter().all(|x| *x == 0));

        for (_, vec) in mat.outer_iterator() {
            for (inner_dim, _) in vec.iter() {
                indptr[inner_dim] += 1;
            }
        }

        let mut cumsum = 0;
        for iptr in indptr.iter_mut() {
            let tmp = *iptr;
            *iptr = cumsum;
            cumsum += tmp;
        }
        if let Some(last_iptr) = indptr.last() {
            assert_eq!(*last_iptr, mat.nb_nonzero());
        }

        for (outer_dim, vec) in mat.outer_iterator() {
            for (inner_dim, val) in vec.iter() {
                let dest = indptr[inner_dim];
                data[dest] = val;
                indices[dest] = outer_dim;
                indptr[inner_dim] += 1;
            }
        }

        let mut last = 0;
        for iptr in indptr.iter_mut() {
            swap(iptr, &mut last);
        }
    }
}

impl<'a, 'b, N, IpStorage, IStorage, DStorage, IpS2, IS2, DS2>
Add<&'b CsMat<N, IpS2, IS2, DS2>>
for &'a CsMat<N, IpStorage, IStorage, DStorage>
where N: 'a + Copy + Num + Default,
      IpStorage: 'a + Deref<Target=[usize]>,
      IStorage: 'a + Deref<Target=[usize]>,
      DStorage: 'a + Deref<Target=[N]>,
      IpS2: 'a + Deref<Target=[usize]>,
      IS2: 'a + Deref<Target=[usize]>,
      DS2: 'a + Deref<Target=[N]> {
    type Output = CsMatOwned<N>;

    fn add(self, rhs: &'b CsMat<N, IpS2, IS2, DS2>) -> CsMatOwned<N> {
        if self.storage() != rhs.borrowed().storage() {
            return binop::add_mat_same_storage(
                self, &rhs.borrowed().to_other_storage()).unwrap()
        }
        binop::add_mat_same_storage(self, rhs).unwrap()
    }
}

impl<'a, 'b, N, IpStorage, IStorage, DStorage, Mat> Sub<&'b Mat>
for &'a CsMat<N, IpStorage, IStorage, DStorage>
where N: 'a + Copy + Num + Default,
      IpStorage: 'a + Deref<Target=[usize]>,
      IStorage: 'a + Deref<Target=[usize]>,
      DStorage: 'a + Deref<Target=[N]>,
      Mat: SpMatView<N> {
    type Output = CsMatOwned<N>;

    fn sub(self, rhs: &'b Mat) -> CsMatOwned<N> {
        if self.storage() != rhs.borrowed().storage() {
            return binop::sub_mat_same_storage(
                self, &rhs.borrowed().to_other_storage()).unwrap()
        }
        binop::sub_mat_same_storage(self, rhs).unwrap()
    }
}

impl<'a, N, IpStorage, IStorage, DStorage> Mul<N>
for &'a CsMat<N, IpStorage, IStorage, DStorage>
where N: 'a + Copy + Num,
      IpStorage: 'a + Deref<Target=[usize]>,
      IStorage: 'a + Deref<Target=[usize]>,
      DStorage: 'a + Deref<Target=[N]> {
    type Output = CsMatOwned<N>;

    fn mul(self, rhs: N) -> CsMatOwned<N> {
        binop::scalar_mul_mat(self, rhs)
    }
}

impl<'a, 'b, N, IpS1, IS1, DS1, IpS2, IS2, DS2>
Mul<&'b CsMat<N, IpS2, IS2, DS2>>
for &'a CsMat<N, IpS1, IS1, DS1>
where N: 'a + Copy + Num + Default,
      IpS1: 'a + Deref<Target=[usize]>,
      IS1: 'a + Deref<Target=[usize]>,
      DS1: 'a + Deref<Target=[N]>,
      IpS2: 'b + Deref<Target=[usize]>,
      IS2: 'b + Deref<Target=[usize]>,
      DS2: 'b + Deref<Target=[N]> {
    type Output = CsMatOwned<N>;

    fn mul(self, rhs: &'b CsMat<N, IpS2, IS2, DS2>) -> CsMatOwned<N> {
        match (self.storage(), rhs.storage()) {
            (CSR, CSR) => {
                let mut workspace = prod::workspace_csr(self, rhs);
                prod::csr_mul_csr(self, rhs, &mut workspace).unwrap()
            }
            (CSR, CSC) => {
                let mut workspace = prod::workspace_csr(self, rhs);
                prod::csr_mul_csr(self,
                                  &rhs.to_other_storage(),
                                  &mut workspace).unwrap()
            }
            (CSC, CSR) => {
                let mut workspace = prod::workspace_csc(self, rhs);
                prod::csc_mul_csc(self, &rhs.to_other_storage(),
                                  &mut workspace).unwrap()
            }
            (CSC, CSC) => {
                let mut workspace = prod::workspace_csc(self, rhs);
                prod::csc_mul_csc(self, rhs, &mut workspace).unwrap()
            }
        }
    }
}

impl<'a, 'b, N, IpS, IS, DS, DS2>
Mul<&'b Tensor<N, [usize; 2], DS2>>
for &'a CsMat<N, IpS, IS, DS>
where N: 'a + Copy + Num + Default,
      IpS: 'a + Deref<Target=[usize]>,
      IS: 'a + Deref<Target=[usize]>,
      DS: 'a + Deref<Target=[N]>,
      DS2: 'b + Deref<Target=[N]> {
    type Output = MatOwned<N>;

    fn mul(self, rhs: &'b Tensor<N, [usize; 2], DS2>) -> MatOwned<N> {
        let rows = self.rows();
        let cols = rhs.cols();
        match (self.storage(), rhs.ordering()) {
            (CSR, StorageOrder::C) => {
                let mut res = Tensor::zeros([rows, cols]);
                prod::csr_mulacc_dense_rowmaj(self.borrowed(), rhs.borrowed(),
                                              res.borrowed_mut()).unwrap();
                res
            }
            (CSR, StorageOrder::F) => {
                let mut res = Tensor::zeros_f([rows, cols]);
                prod::csr_mulacc_dense_colmaj(self.borrowed(), rhs.borrowed(),
                                              res.borrowed_mut()).unwrap();
                res
            }
            (CSC, StorageOrder::C) => {
                let mut res = Tensor::zeros([rows, cols]);
                prod::csc_mulacc_dense_rowmaj(self.borrowed(), rhs.borrowed(),
                                              res.borrowed_mut()).unwrap();
                res
            }
            (CSC, StorageOrder::F) => {
                let mut res = Tensor::zeros_f([rows, cols]);
                prod::csc_mulacc_dense_colmaj(self.borrowed(), rhs.borrowed(),
                                              res.borrowed_mut()).unwrap();
                res
            }
            (_, StorageOrder::Unordered) => unreachable!("mats are ordered")
        }
    }
}

impl<'a, 'b, N, IpS, IS, DS, DS2>
Add<&'b Tensor<N, [usize; 2], DS2>>
for &'a CsMat<N, IpS, IS, DS>
where N: 'a + Copy + Num + Default,
      IpS: 'a + Deref<Target=[usize]>,
      IS: 'a + Deref<Target=[usize]>,
      DS: 'a + Deref<Target=[N]>,
      DS2: 'b + Deref<Target=[N]> {
    type Output = MatOwned<N>;

    fn add(self, rhs: &'b Tensor<N, [usize; 2], DS2>) -> MatOwned<N> {
        match (self.storage(), rhs.ordering()) {
            (CSR, StorageOrder::C) => {
                    binop::add_dense_mat_same_ordering(self,
                                                       rhs,
                                                       N::one(),
                                                       N::one()).unwrap()
                }
                (CSR, StorageOrder::F) => {
                    let lhs = self.to_other_storage(); 
                    binop::add_dense_mat_same_ordering(&lhs,
                                                       rhs,
                                                       N::one(),
                                                       N::one()).unwrap()
                }
                (CSC, StorageOrder::C) => {
                    let lhs = self.to_other_storage(); 
                    binop::add_dense_mat_same_ordering(&lhs,
                                                       rhs,
                                                       N::one(),
                                                       N::one()).unwrap()
                }
                (CSC, StorageOrder::F) => {
                    binop::add_dense_mat_same_ordering(self,
                                                       rhs,
                                                       N::one(),
                                                       N::one()).unwrap()
                }
                (_, StorageOrder::Unordered) => unreachable!("mats are ordered")
        }
    }
}

/// An iterator over non-overlapping blocks of a matrix,
/// along the least-varying dimension
pub struct ChunkOuterBlocks<'a, N: 'a> {
    mat: CsMatView<'a, N>,
    dims_in_bloc: usize,
    bloc_count: usize,
}

impl<'a, N: 'a + Copy> Iterator for ChunkOuterBlocks<'a, N> {
    type Item = CsMatView<'a, N>;
    fn next(&mut self) -> Option<<Self as Iterator>::Item> {
        let cur_dim = self.dims_in_bloc * self.bloc_count;
        let end_dim = self.dims_in_bloc + cur_dim;
        let count = if self.dims_in_bloc == 0 {
            return None;
        }
        else if end_dim > self.mat.outer_dims() {
            let count = self.mat.outer_dims() - cur_dim;
            self.dims_in_bloc = 0;
            count
        }
        else {
            self.dims_in_bloc
        };
        let view = self.mat.middle_outer_views(cur_dim,
                                               count).unwrap();
        self.bloc_count += 1;
        Some(view)
    }
}


#[cfg(test)]
mod test {
    use super::{CsMat, CsMatOwned};
    use super::CompressedStorage::{CSC, CSR};
    use errors::SprsError;
    use test_data::{mat1, mat1_csc, mat1_times_2};

    #[test]
    fn test_new_csr_success() {
        let indptr_ok : &[usize] = &[0, 1, 2, 3];
        let indices_ok : &[usize] = &[0, 1, 2];
        let data_ok : &[f64] = &[1., 1., 1.];
        let m = CsMat::new_borrowed(CSR, 3, 3, indptr_ok, indices_ok, data_ok);
        assert!(m.is_ok());
    }

    #[test]
    fn test_new_csr_fails() {
        let indptr_ok : &[usize] = &[0, 1, 2, 3];
        let indices_ok : &[usize] = &[0, 1, 2];
        let data_ok : &[f64] = &[1., 1., 1.];
        let indptr_fail1 : &[usize] = &[0, 1, 2];
        let indptr_fail2 : &[usize] = &[0, 1, 2, 4];
        let indptr_fail3 : &[usize] = &[0, 2, 1, 3];
        let indices_fail1 : &[usize] = &[0, 1];
        let indices_fail2 : &[usize] = &[0, 1, 4];
        let data_fail1 : &[f64] = &[1., 1., 1., 1.];
        let data_fail2 : &[f64] = &[1., 1.,];
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3, indptr_fail1,
                                      indices_ok, data_ok),
                   Err(SprsError::BadIndptrLength));
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3,
                                      indptr_fail2, indices_ok, data_ok),
                   Err(SprsError::OutOfBoundsIndptr));
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3,
                                      indptr_fail3, indices_ok, data_ok),
                   Err(SprsError::UnsortedIndptr));
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3,
                                      indptr_ok, indices_fail1, data_ok),
                   Err(SprsError::DataIndicesMismatch));
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3,
                                      indptr_ok, indices_fail2, data_ok),
                   Err(SprsError::OutOfBoundsIndex));
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3,
                                      indptr_ok, indices_ok, data_fail1),
                   Err(SprsError::DataIndicesMismatch));
        assert_eq!(CsMat::new_borrowed(CSR, 3, 3,
                                      indptr_ok, indices_ok, data_fail2),
                   Err(SprsError::DataIndicesMismatch));
    }

    #[test]
    fn test_new_csr_fail_indices_ordering() {
        let indptr: &[usize] = &[0, 2, 4, 5, 6, 7];
        // good indices would be [2, 3, 3, 4, 2, 1, 3];
        let indices: &[usize] = &[3, 2, 3, 4, 2, 1, 3];
        let data: &[f64] = &[
            0.35310881, 0.42380633, 0.28035896, 0.58082095,
            0.53350123, 0.88132896, 0.72527863];
        assert_eq!(CsMat::new_borrowed(CSR, 5, 5,
                                      indptr, indices, data),
                   Err(SprsError::NonSortedIndices));
    }

    #[test]
    fn test_new_csr_csc_success() {
        let indptr_ok : &[usize] = &[0, 2, 5, 6];
        let indices_ok : &[usize] = &[2, 3, 1, 2, 3, 3];
        let data_ok : &[f64] = &[
            0.05734571, 0.15543348, 0.75628258,
            0.83054515, 0.71851547, 0.46202352];
        assert!(CsMat::new_borrowed(CSR, 3, 4,
                                   indptr_ok, indices_ok, data_ok).is_ok());
        assert!(CsMat::new_borrowed(CSC, 4, 3,
                                   indptr_ok, indices_ok, data_ok).is_ok());
    }

    #[test]
    fn test_new_csr_csc_fails() {
        let indptr_ok : &[usize] = &[0, 2, 5, 6];
        let indices_ok : &[usize] = &[2, 3, 1, 2, 3, 3];
        let data_ok : &[f64] = &[
            0.05734571, 0.15543348, 0.75628258,
            0.83054515, 0.71851547, 0.46202352];
        assert_eq!(CsMat::new_borrowed(CSR, 4, 3,
                                      indptr_ok, indices_ok, data_ok),
                   Err(SprsError::BadIndptrLength));
        assert_eq!(CsMat::new_borrowed(CSC, 3, 4,
                                      indptr_ok, indices_ok, data_ok),
                   Err(SprsError::BadIndptrLength));
    }


    #[test]
    fn test_new_csr_vec_borrowed() {
        let indptr_ok = vec![0, 1, 2, 3];
        let indices_ok = vec![0, 1, 2];
        let data_ok : Vec<f64> = vec![1., 1., 1.];
        assert!(CsMat::new_borrowed(CSR, 3, 3,
                                   &indptr_ok, &indices_ok, &data_ok).is_ok());
    }

    #[test]
    fn test_new_csr_vec_owned() {
        let indptr_ok = vec![0, 1, 2, 3];
        let indices_ok = vec![0, 1, 2];
        let data_ok : Vec<f64> = vec![1., 1., 1.];
        assert!(CsMat::new_owned(CSR, 3, 3,
                                 indptr_ok, indices_ok, data_ok).is_ok());
    }

    #[test]
    fn new_csr_with_empty_row() {
        let indptr: &[usize] = &[0, 3, 3, 5, 6, 7];
        let indices: &[usize] = &[1, 2, 3, 2, 3, 4, 4];
        let data: &[f64] = &[
            0.75672424, 0.1649078, 0.30140296, 0.10358244,
            0.6283315, 0.39244208, 0.57202407];
        assert!(CsMat::new_borrowed(CSR, 5, 5, indptr, indices, data).is_ok());
    }

    #[test]
    fn csr_to_csc() {
        let a = mat1();
        let a_csc_ground_truth = mat1_csc();
        let a_csc = a.to_other_storage();
        assert_eq!(a_csc, a_csc_ground_truth);
    }

    #[test]
    fn test_self_smul() {
        let mut a = mat1();
        a.scale(2.);
        let c_true = mat1_times_2();
        assert_eq!(a.indptr(), c_true.indptr());
        assert_eq!(a.indices(), c_true.indices());
        assert_eq!(a.data(), c_true.data());
    }

    #[test]
    fn outer_block_iter() {
        let mat : CsMatOwned<f64> = CsMat::eye(CSR, 11);
        let mut block_iter = mat.outer_block_iter(3);
        assert_eq!(block_iter.next().unwrap().rows(), 3);
        assert_eq!(block_iter.next().unwrap().rows(), 3);
        assert_eq!(block_iter.next().unwrap().rows(), 3);
        assert_eq!(block_iter.next().unwrap().rows(), 2);
        assert_eq!(block_iter.next(), None);

        let mut block_iter = mat.outer_block_iter(4);
        assert_eq!(block_iter.next().unwrap().cols(), 11);
        block_iter.next().unwrap();
        block_iter.next().unwrap();
        assert_eq!(block_iter.next(), None);
    }
}
