Var TransactionActive;

Procedure ProcessItems(Items)
    For Each Item In Items Do
        If Item.Skip Then
            Continue;
        EndIf;
        BeginTransaction();
        Try
            Process(Item);
            CommitTransaction();
        Except
            If TransactionActive() Then
                RollbackTransaction();
            EndIf;
            Raise;
        EndTry;
        Return;
    EndDo;
EndProcedure

Function ResultAfterRollback(TransactionActive)
    BeginTransaction();
    Try
        CommitTransaction();
    Except
        If Not TransactionActive() Then
            Return False;
        Else
            RollbackTransaction();
        EndIf;
        Return False;
    EndTry;
    Return True;
EndFunction
