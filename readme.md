# OpenOMS

**TODO:**



**1) Order Groups**
- add a `order_group_id` to indicate contracts that belong to one trade
  e.g. say a user wants to trade a straddle - then the use would 
  want to know which orders belong together afterwards (long Call / Put)
  => how would that change the status of an order? would it be partially filled or not?
  => does it even make sense to model composite trades in the oms - or should there
     be responsibility to model the composite trade be handled on the client side?


**2) Instrument Mappings**
- add broker/exchange <-> master instrument mapping 
    * this also means that there is some sort of interface such that users 
      can add new instrument mappings
- rethink whether the authentication should be based on principal or on account
- should we add an omnibus account? <= well we kinda have that already since we are using one 
  base account for the external broker connection/api
