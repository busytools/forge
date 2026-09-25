//! Review threads and submitted reviews for one branch.

use forge_primitives::ReviewSet;
use forge_primitives::review::ReviewThread;

use super::ViewSurface;

/// Everything the `/diff` overlay reads for one `(project, branch)`.
///
/// Two Results rather than one, because the two live in separate redb
/// tables (`review_threads` and `reviews`) and fail independently
/// today. Collapsing them would let a corrupt reviews row start failing
/// a threads-only reader.
pub struct ReviewsView {
    pub threads: Result<Vec<ReviewThread>, String>,
    pub reviews: Result<Vec<ReviewSet>, String>,
}

impl ViewSurface {
    pub fn reviews(&self, project: &str, branch: &str) -> ReviewsView {
        ReviewsView {
            threads: self.workspace.load_review_threads(project, branch),
            reviews: self.workspace.load_reviews(project, branch),
        }
    }
}
