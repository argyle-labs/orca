/// Returns an iterator yielding Fibonacci numbers up to n.
/// 
/// # Examples
/// ```
/// let fib = fib_sequence(5);
/// for f in fib {
///     println!("{}", f);
/// }
/// // Outputs: 0 1 1 2 3 5
/// 
/// let nums: Vec<u64> = (0..fib_sequence(8)).collect();
/// println!("{:?}", nums);
/// // Outputs: [0, 1, 1, 2, 3, 5, 8]
/// ```
pub fn fib_sequence(n: u64) -> impl Iterator<Item = u64> {
    let mut a = 0u64;
    let mut b = 1u64;
    let limit = if n == 0 { 0 } else { n };

    std::iter::from_fn(move || {
        if a <= limit {
            let result = a;
            let next = a + b;
            let temp = b;
            b = a + b;
            a = temp;
            Some(result)
        } else {
            None
        }
    })
}

// Test with main for quick verification
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic() {
        let fib: Vec<u64> = (0..fib_sequence(8)).collect();
        assert_eq!(fib, vec![0, 1, 1, 2, 3, 5, 8]);
    }

    #[test]
    fn test_zero() {
        let fib: Vec<u64> = (0..fib_sequence(0)).collect();
        assert!(fib.is_empty());
    }

    #[test]
    fn test_single() {
        let fib: Vec<u64> = (0..fib_sequence(1)).collect();
        assert_eq!(fib, vec![0, 1]);
    }
}

fn main() {
    let n = 10;
    println!("Fibonacci sequence up to {}: ", n);
    for f in fib_sequence(n) {
        println!("{}", f);
    }
}