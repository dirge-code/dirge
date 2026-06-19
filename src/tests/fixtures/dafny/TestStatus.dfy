module TestTypes {
  // TestStatus datatype representing the possible states of a test
  datatype TestStatus = 
    | OVERRUN
    | INVALID  
    | VALID
    | INTERESTING

  datatype MinithesisException = 
    | UnsatisfiableTestCaseException
    | OverrunException  
    | InvalidTestCaseException
}
