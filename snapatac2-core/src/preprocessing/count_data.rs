mod import;
mod coverage;
mod genome;
mod matrix;

pub use crate::preprocessing::qc;
pub use import::{import_fragments, import_contacts};
pub use coverage::{GenomeCount, ContactMap, FragmentType, fragments_to_insertions};
pub use genome::{
    TranscriptParserOptions, Transcript, Promoters, FeatureCounter, TranscriptCount, GeneCount,
    read_transcripts_from_gff, read_transcripts_from_gtf,
    ChromSizes, ChromValueIter, ChromValues, GenomeBaseIndex, 
};
pub use matrix::{create_gene_matrix, create_tile_matrix, create_peak_matrix};

use anndata::{container::{ChunkedArrayElem, StackedChunkedArrayElem}, ArrayElemOp};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use bed_utils::bed::{tree::BedTree, GenomicRange};
use anndata::{AnnDataOp, ElemCollectionOp, AxisArraysOp, AnnDataSet, Backend, AnnData};
use ndarray::Array2;
use polars::frame::DataFrame;
use nalgebra_sparse::CsrMatrix;
use anyhow::{Result, Context, bail};
use num::integer::div_ceil;
use std::str::FromStr;

/// The `SnapData` trait represents an interface for reading and
/// manipulating single-cell assay data. It extends the `AnnDataOp` trait,
/// adding methods for reading chromosome sizes and genome-wide base-resolution coverage.
pub trait SnapData: AnnDataOp {
    type CountIter: ExactSizeIterator<Item = (CsrMatrix<u8>, usize, usize)>;

    /// Return chromosome names and sizes.
    fn read_chrom_sizes(&self) -> Result<ChromSizes> {
        let df = self
            .uns()
            .get_item::<DataFrame>("reference_sequences")?
            .context("key 'reference_sequences' is not present in the '.uns'")?;
        let chrs = df.column("reference_seq_name").unwrap().utf8()?;
        let chr_sizes = df.column("reference_seq_length").unwrap().u64()?;
        let res = chrs.into_iter().flatten().map(|x| x.to_string())
            .zip(chr_sizes.into_iter().flatten()).collect();
        Ok(res)
    }

    /// Read insertion counts stored in the `.obsm` matrix.
    fn get_count_iter(&self, chunk_size: usize) ->
        Result<GenomeCount<Box<dyn ExactSizeIterator<Item = (FragmentType, usize, usize)>>>>;

    fn contact_count_iter(&self, chunk_size: usize) -> Result<ContactMap<Self::CountIter>>;

    /// Read counts stored in the `X` matrix.
    fn read_chrom_values(
        &self,
        chunk_size: usize,
    ) -> Result<ChromValueIter<<<Self as AnnDataOp>::X as ArrayElemOp>::ArrayIter<CsrMatrix<u32>>>>
    {
        let regions = self
            .var_names()
            .into_vec()
            .into_iter()
            .map(|x| GenomicRange::from_str(x.as_str()).unwrap())
            .collect();
        Ok(ChromValueIter {
            regions,
            iter: self.x().iter(chunk_size),
            length: div_ceil(self.n_obs(), chunk_size),
        })
    }

    /// QC metrics for the data.

    /// Compute TSS enrichment.
    fn tss_enrichment(&self, promoter: &BedTree<bool>) -> Result<Vec<f64>> {
        Ok(self.get_count_iter(2000)?.into_fragments().flat_map(|(fragments, _, _)| {
            fragments.into_par_iter()
                .map(|x| qc::tss_enrichment(x.into_iter(), promoter))
                .collect::<Vec<_>>()
        }).collect())
    }

    /// Compute the fragment size distribution.
    fn fragment_size_distribution(&self, max_size: usize) -> Result<Vec<usize>>;

    /// Compute the fraction of reads in each region.
    fn frip<D>(&self, regions: &Vec<BedTree<D>>) -> Result<Array2<f64>> {
        let vec = qc::fraction_of_reads_in_region(self.get_count_iter(2000)?.into_fragments(), regions)
            .map(|x| x.0).flatten().flatten().collect::<Vec<_>>();
        Array2::from_shape_vec((self.n_obs(), regions.len()), vec).map_err(Into::into)
    }
}

impl<B: Backend> SnapData for AnnData<B> {
    type CountIter = ChunkedArrayElem<B, CsrMatrix<u8>>;

    fn get_count_iter(&self, chunk_size: usize) ->
        Result<GenomeCount<Box<dyn ExactSizeIterator<Item = (FragmentType, usize, usize)>>>>
    {
        let obsm = self.obsm();
        let matrices: Box<dyn ExactSizeIterator<Item = (FragmentType, usize, usize)>> =
            if let Some(insertion) = obsm.get_item_iter("fragment_single", chunk_size) {
                Box::new(insertion.map(|(x, a, b)| (FragmentType::FragmentSingle(x), a, b)))
            } else if let Some(fragment) = obsm.get_item_iter("fragment_paired", chunk_size) {
                Box::new(fragment.map(|(x, a, b)| (FragmentType::FragmentPaired(x), a, b)))
            } else {
                anyhow::bail!("neither 'fragment_single' nor 'fragment_paired' is present in the '.obsm'")
            };
        Ok(GenomeCount::new(self.read_chrom_sizes()?, matrices))
    }

    fn contact_count_iter(&self, chunk_size: usize) -> Result<ContactMap<Self::CountIter>> {
        Ok(ContactMap::new(
            self.read_chrom_sizes()?,
            self.obsm().get_item_iter("contact", chunk_size).unwrap(),
        ))
    }

    fn fragment_size_distribution(&self, max_size: usize) -> Result<Vec<usize>> {
        if let Some(fragment) = self.obsm().get_item_iter("fragment_paired", 500) {
            Ok(qc::fragment_size_distribution(fragment.map(|x| x.0), max_size))
        } else {
            bail!("key 'fragment_paired' is not present in the '.obsm'")
        }
    }
}

impl<B: Backend> SnapData for AnnDataSet<B> {
    type CountIter = StackedChunkedArrayElem<B, CsrMatrix<u8>>;

    fn get_count_iter(&self, chunk_size: usize) ->
        Result<GenomeCount<Box<dyn ExactSizeIterator<Item = (FragmentType, usize, usize)>>>>
    {
        let adatas = self.adatas().inner();
        let obsm = adatas.get_obsm();
        let matrices: Box<dyn ExactSizeIterator<Item = (FragmentType, usize, usize)>> =
            if let Some(insertion) = obsm.get_item_iter("fragment_single", chunk_size) {
                Box::new(insertion.map(|(x, a, b)| (FragmentType::FragmentSingle(x), a, b)))
            } else if let Some(fragment) = obsm.get_item_iter("fragment_paired", chunk_size) {
                Box::new(fragment.map(|(x, a, b)| (FragmentType::FragmentPaired(x), a, b)))
            } else {
                anyhow::bail!("neither 'fragment_single' nor 'fragment_paired' is present in the '.obsm'")
            };
        Ok(GenomeCount::new(self.read_chrom_sizes()?, matrices))
    }

    fn contact_count_iter(&self, chunk_size: usize) -> Result<ContactMap<Self::CountIter>> {
        Ok(ContactMap::new(
            self.read_chrom_sizes()?,
            self.adatas()
                .inner()
                .get_obsm()
                .get_item_iter("contact", chunk_size)
                .unwrap(),
        ))
    }

    fn fragment_size_distribution(&self, max_size: usize) -> Result<Vec<usize>> {
        if let Some(fragment) = self.adatas().inner().get_obsm().get_item_iter("fragment_paired", 500) {
            Ok(qc::fragment_size_distribution(fragment.map(|x| x.0), max_size))
        } else {
            bail!("key 'fragment_paired' is not present in the '.obsm'")
        }
    }
}