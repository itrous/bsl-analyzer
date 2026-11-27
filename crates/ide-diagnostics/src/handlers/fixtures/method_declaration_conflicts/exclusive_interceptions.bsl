#Если Сервер Тогда
&После("ПередЗаписью")
Процедура НаСервере()
КонецПроцедуры
#Иначе
&После("ПередЗаписью")
Процедура НаКлиенте()
КонецПроцедуры
#КонецЕсли

&Before("FirstTarget")
Procedure First()
EndProcedure

&Before("OtherTarget")
Procedure Second()
EndProcedure

&After("FirstTarget")
Procedure Third()
EndProcedure
