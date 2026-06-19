include "./TestStatus.dfy"
module TestResults {
  import opened TestTypes
  import opened Std.Wrappers

  // TestResult class representing the result of a test with either an error status or a value
  class TestResult<T> {
    var err: Option<TestStatus>
    var value: Option<T>

    constructor(error: Option<TestStatus>, value: Option<T>) 
      ensures this.err == error && this.value == value
    {
      this.err := error;
      this.value := value;
    }



    // Unwrap the value, throwing an exception if there's no value
    function Unwrap(): T
      requires this.value.Some?
      reads this
    {
        this.value.Extract()
    }


    // Check if the result is valid
    function IsValid(): bool
     reads this
    {
      this.err == None || this.err == Some(TestStatus.INTERESTING)
    }

    // Get the error status
    function Error(): Option<TestStatus>
      reads this
    {
      this.err
    }

    // Map function over the result
    method Map<R>(f: T -> R) returns (res: TestResult<R> ) {
      match this.value {
        case Some(v) => res := new TestResult<R>(None, Some(f(v)));
        case None => res := new TestResult<R>(this.err, None);
        }
      }

//   // Static factory methods as methods instead of functions
  static method TestResultError<T>(s: TestStatus) returns (res: TestResult<T> )
  {
    res := new TestResult<T>(Some(s), None);
  }

  static method TestResultSuccess<T>(t: T) returns (res: TestResult<T> )
  {
    res := new TestResult<T>(None, Some(t));
  }
}
}